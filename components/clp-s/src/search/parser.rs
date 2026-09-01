use super::BooleanOperator;
use super::ColumnNamespace;
use super::ColumnPath;
use super::ComparisonOperator;
use super::ExpressionKind;
use super::ExpressionNode;
use super::KqlError;
use super::KqlExpected;
use super::KqlLimits;
use super::KqlResource;
use super::KqlToken;
use super::KqlTokenKind;
use super::ListExpression;
use super::ListOperator;
use super::Literal;
use super::NodeId;
use super::ParsedQuery;
use super::PathComponent;
use super::Predicate;
use super::SourceSpan;
use super::StringLiteral;
use super::TimestampLiteral;
use super::lex_kql;

/// Parses the pinned CLP-S KQL dialect into an owned archive-independent node arena.
///
/// `NOT` binds more tightly than binary operators. Unlike conventional Boolean grammars, `AND`
/// and `OR` intentionally have equal precedence and associate left-to-right. Nested `key:{...}`
/// paths are fully prefixed during parsing, while the compact list form remains unexpanded.
///
/// # Errors
///
/// Returns a byte-offset-bearing [`KqlError`] for lexical or syntactic incompatibility, invalid
/// value/path escapes, nested namespace conflicts, resource limits, arithmetic overflow, or failed
/// checked allocations.
pub fn parse_kql(input: &str, limits: KqlLimits) -> Result<ParsedQuery, KqlError> {
    let tokens = lex_kql(input, limits)?;
    Parser::new(input, &tokens, limits).parse()
}

struct Parser<'input, 'tokens> {
    input: &'input str,
    tokens: &'tokens [KqlToken],
    cursor: usize,
    limits: KqlLimits,
    builder: AstBuilder,
    values: Vec<NodeId>,
    operators: Vec<PendingOperator>,
    active_prefix: Vec<PathComponent>,
    active_namespace: ColumnNamespace,
    nested_depth: usize,
    syntax_depth: usize,
}

impl<'input, 'tokens> Parser<'input, 'tokens> {
    const fn new(input: &'input str, tokens: &'tokens [KqlToken], limits: KqlLimits) -> Self {
        Self {
            input,
            tokens,
            cursor: 0,
            limits,
            builder: AstBuilder::new(limits),
            values: Vec::new(),
            operators: Vec::new(),
            active_prefix: Vec::new(),
            active_namespace: ColumnNamespace::Default,
            nested_depth: 0,
            syntax_depth: 0,
        }
    }

    fn parse(mut self) -> Result<ParsedQuery, KqlError> {
        let mut expect_operand = true;
        while let Some(token) = self.current() {
            if expect_operand {
                match token.kind {
                    KqlTokenKind::Not => {
                        self.push_operator(PendingOperator::Not {
                            offset: token.span.start(),
                        })?;
                        self.cursor += 1;
                    }
                    KqlTokenKind::LeftParenthesis => {
                        self.open_group(GroupKind::Parenthesis, token.span.start())?;
                        self.cursor += 1;
                    }
                    KqlTokenKind::Literal { .. } => {
                        if self.parse_literal_operand()? {
                            expect_operand = false;
                        }
                    }
                    _ => {
                        return Err(self.unexpected(KqlExpected::Expression, Some(token)));
                    }
                }
            } else {
                match token.kind {
                    KqlTokenKind::And | KqlTokenKind::Or => {
                        self.reduce_binary_until_group(token.span.start())?;
                        let operator = if token.kind == KqlTokenKind::And {
                            BooleanOperator::And
                        } else {
                            BooleanOperator::Or
                        };
                        self.push_operator(PendingOperator::Binary {
                            operator,
                            offset: token.span.start(),
                        })?;
                        self.cursor += 1;
                        expect_operand = true;
                    }
                    KqlTokenKind::RightParenthesis | KqlTokenKind::RightBrace => {
                        self.close_group(token)?;
                        self.cursor += 1;
                    }
                    _ => {
                        return Err(self.unexpected(KqlExpected::BooleanOperator, Some(token)));
                    }
                }
            }
        }

        if expect_operand {
            return Err(self.unexpected(KqlExpected::Expression, None));
        }
        self.finish_operators()?;
        if self.values.len() != 1 {
            return Err(KqlError::MalformedExpression {
                offset: self.input.len(),
            });
        }
        let root = self.values.pop().ok_or(KqlError::MalformedExpression {
            offset: self.input.len(),
        })?;
        Ok(ParsedQuery {
            nodes: self.builder.nodes,
            root,
        })
    }

    fn parse_literal_operand(&mut self) -> Result<bool, KqlError> {
        let token = self
            .current()
            .ok_or_else(|| self.unexpected(KqlExpected::Expression, None))?;
        let next = self.tokens.get(self.cursor + 1).copied();
        let Some(operator_token) = next.filter(|next| is_column_operator(next.kind)) else {
            let literal = self.parse_literal(token)?;
            let path = self.default_wildcard_path(token.span.start())?;
            let node = self.builder.push_leaf(
                ExpressionKind::Predicate(Predicate {
                    path,
                    operator: ComparisonOperator::Equal,
                    value: literal,
                }),
                token.span,
            )?;
            self.push_value(node, token.span.start())?;
            self.cursor += 1;
            self.reduce_unary()?;
            return Ok(true);
        };

        let path = self.parse_column(token)?;
        match operator_token.kind {
            KqlTokenKind::Colon => self.parse_equal_operand(token, path),
            KqlTokenKind::Less
            | KqlTokenKind::LessOrEqual
            | KqlTokenKind::Greater
            | KqlTokenKind::GreaterOrEqual => self.parse_range_operand(token, path, operator_token),
            _ => Err(KqlError::MalformedExpression {
                offset: operator_token.span.start(),
            }),
        }
    }

    fn parse_equal_operand(
        &mut self,
        column_token: KqlToken,
        path: ColumnPath,
    ) -> Result<bool, KqlError> {
        let value_index = self.cursor + 2;
        let value_token = self
            .tokens
            .get(value_index)
            .copied()
            .ok_or_else(|| self.unexpected_at(self.input.len(), KqlExpected::Value, None))?;
        if value_token.kind == KqlTokenKind::LeftBrace {
            self.enter_nested(path, column_token.span.start(), value_token.span.start())?;
            self.cursor = value_index + 1;
            return Ok(false);
        }
        if value_token.kind == KqlTokenKind::LeftParenthesis {
            let (node, next_index) =
                self.parse_list(path, column_token.span.start(), value_index + 1)?;
            self.push_value(node, column_token.span.start())?;
            self.cursor = next_index;
            self.reduce_unary()?;
            return Ok(true);
        }

        let path = self.finish_path(path, column_token.span.start())?;
        let (value, value_span, next_index) = self.parse_value(value_index)?;
        let node = self.builder.push_leaf(
            ExpressionKind::Predicate(Predicate {
                path,
                operator: ComparisonOperator::Equal,
                value,
            }),
            SourceSpan::new(column_token.span.start(), value_span.end()),
        )?;
        self.push_value(node, column_token.span.start())?;
        self.cursor = next_index;
        self.reduce_unary()?;
        Ok(true)
    }

    fn parse_range_operand(
        &mut self,
        column_token: KqlToken,
        path: ColumnPath,
        operator_token: KqlToken,
    ) -> Result<bool, KqlError> {
        let value_index = self.cursor + 2;
        let path = self.finish_path(path, column_token.span.start())?;
        let (value, value_span, next_index) = self.parse_value(value_index)?;
        let operator = match operator_token.kind {
            KqlTokenKind::Less => ComparisonOperator::Less,
            KqlTokenKind::LessOrEqual => ComparisonOperator::LessOrEqual,
            KqlTokenKind::Greater => ComparisonOperator::Greater,
            KqlTokenKind::GreaterOrEqual => ComparisonOperator::GreaterOrEqual,
            _ => {
                return Err(KqlError::MalformedExpression {
                    offset: operator_token.span.start(),
                });
            }
        };
        let node = self.builder.push_leaf(
            ExpressionKind::Predicate(Predicate {
                path,
                operator,
                value,
            }),
            SourceSpan::new(column_token.span.start(), value_span.end()),
        )?;
        self.push_value(node, column_token.span.start())?;
        self.cursor = next_index;
        self.reduce_unary()?;
        Ok(true)
    }

    fn parse_value(&mut self, index: usize) -> Result<(Literal, SourceSpan, usize), KqlError> {
        let token = self
            .tokens
            .get(index)
            .copied()
            .ok_or_else(|| self.unexpected_at(self.input.len(), KqlExpected::Value, None))?;
        match token.kind {
            KqlTokenKind::Literal { .. } => Ok((self.parse_literal(token)?, token.span, index + 1)),
            KqlTokenKind::TimestampStart => self.parse_timestamp(index),
            _ => Err(self.unexpected_at(token.span.start(), KqlExpected::Value, Some(token))),
        }
    }

    fn parse_timestamp(
        &mut self,
        start_index: usize,
    ) -> Result<(Literal, SourceSpan, usize), KqlError> {
        let start = self
            .tokens
            .get(start_index)
            .copied()
            .ok_or(KqlError::MalformedExpression {
                offset: self.input.len(),
            })?;
        let value_token = self.tokens.get(start_index + 1).copied().ok_or_else(|| {
            self.unexpected_at(self.input.len(), KqlExpected::QuotedTimestamp, None)
        })?;
        if value_token.kind != (KqlTokenKind::Literal { quoted: true }) {
            return Err(self.unexpected_at(
                value_token.span.start(),
                KqlExpected::QuotedTimestamp,
                Some(value_token),
            ));
        }
        let value = self.copy_quoted_raw(value_token)?;
        let separator = self.tokens.get(start_index + 2).copied().ok_or_else(|| {
            self.unexpected_at(self.input.len(), KqlExpected::TimestampSeparator, None)
        })?;

        let (pattern, close_index) = match separator.kind {
            KqlTokenKind::RightParenthesis => (None, start_index + 2),
            KqlTokenKind::Comma => {
                let pattern_token = self.tokens.get(start_index + 3).copied().ok_or_else(|| {
                    self.unexpected_at(self.input.len(), KqlExpected::QuotedTimestamp, None)
                })?;
                if pattern_token.kind != (KqlTokenKind::Literal { quoted: true }) {
                    return Err(self.unexpected_at(
                        pattern_token.span.start(),
                        KqlExpected::QuotedTimestamp,
                        Some(pattern_token),
                    ));
                }
                let pattern = self.copy_quoted_raw(pattern_token)?;
                let close = self.tokens.get(start_index + 4).copied().ok_or_else(|| {
                    self.unexpected_at(self.input.len(), KqlExpected::ClosingParenthesis, None)
                })?;
                if close.kind != KqlTokenKind::RightParenthesis {
                    return Err(self.unexpected_at(
                        close.span.start(),
                        KqlExpected::ClosingParenthesis,
                        Some(close),
                    ));
                }
                (Some(pattern), start_index + 4)
            }
            _ => {
                return Err(self.unexpected_at(
                    separator.span.start(),
                    KqlExpected::TimestampSeparator,
                    Some(separator),
                ));
            }
        };
        let close = self
            .tokens
            .get(close_index)
            .copied()
            .ok_or(KqlError::MalformedExpression {
                offset: self.input.len(),
            })?;
        Ok((
            Literal::Timestamp(TimestampLiteral { value, pattern }),
            SourceSpan::new(start.span.start(), close.span.end()),
            close_index + 1,
        ))
    }

    fn parse_list(
        &mut self,
        path: ColumnPath,
        expression_start: usize,
        mut index: usize,
    ) -> Result<(NodeId, usize), KqlError> {
        let path = self.finish_path(path, expression_start)?;
        let mut operator = ListOperator::Any;
        if let Some(token) = self.tokens.get(index) {
            operator = match token.kind {
                KqlTokenKind::And => ListOperator::All,
                KqlTokenKind::Or => ListOperator::Any,
                KqlTokenKind::Not => ListOperator::None,
                _ => operator,
            };
            if matches!(
                token.kind,
                KqlTokenKind::And | KqlTokenKind::Or | KqlTokenKind::Not
            ) {
                index += 1;
            }
        }

        let mut values = Vec::new();
        loop {
            let token = self.tokens.get(index).copied().ok_or_else(|| {
                self.unexpected_at(self.input.len(), KqlExpected::ClosingParenthesis, None)
            })?;
            if token.kind == KqlTokenKind::RightParenthesis {
                let node = self.builder.push_leaf(
                    ExpressionKind::List(ListExpression {
                        path,
                        operator,
                        values,
                    }),
                    SourceSpan::new(expression_start, token.span.end()),
                )?;
                return Ok((node, index + 1));
            }
            if !matches!(token.kind, KqlTokenKind::Literal { .. }) {
                return Err(self.unexpected_at(
                    token.span.start(),
                    KqlExpected::Literal,
                    Some(token),
                ));
            }
            if values.len() >= self.limits.max_list_values() {
                return Err(KqlError::ListLimitExceeded {
                    offset: token.span.start(),
                    limit: self.limits.max_list_values(),
                });
            }
            reserve_one(&mut values, token.span.start(), KqlResource::ListValues)?;
            values.push(self.parse_literal(token)?);
            index += 1;
        }
    }

    fn parse_literal(&mut self, token: KqlToken) -> Result<Literal, KqlError> {
        let (raw, offset) = self.literal_content(token)?;
        let decoded = self.decode_kql(raw, offset, EscapeContext::Value)?;
        if let Ok(value) = decoded.parse::<i64>() {
            return Ok(Literal::Integer {
                value,
                source: decoded,
            });
        }
        if let Ok(value) = decoded.parse::<f64>()
            && value.is_finite()
        {
            return Ok(Literal::Float {
                value,
                source: decoded,
            });
        }
        match decoded.as_str() {
            "true" => Ok(Literal::Boolean(true)),
            "false" => Ok(Literal::Boolean(false)),
            "null" => Ok(Literal::Null),
            _ => {
                let wildcard_pattern = self.clean_wildcards(&decoded, offset)?;
                Ok(Literal::String(StringLiteral { wildcard_pattern }))
            }
        }
    }

    fn parse_column(&mut self, token: KqlToken) -> Result<ColumnPath, KqlError> {
        let (raw, raw_offset) = self.literal_content(token)?;
        let (namespace, content, content_offset) = match raw.as_bytes().first() {
            Some(b'@') => (ColumnNamespace::Autogenerated, &raw[1..], raw_offset + 1),
            Some(b'$') => (ColumnNamespace::RangeIndex, &raw[1..], raw_offset + 1),
            Some(b'!') => (ColumnNamespace::ReservedBang, &raw[1..], raw_offset + 1),
            Some(b'#') => (ColumnNamespace::ReservedHash, &raw[1..], raw_offset + 1),
            _ => (ColumnNamespace::Default, raw, raw_offset),
        };
        if content.is_empty() {
            return Err(KqlError::EmptyPathComponent {
                offset: content_offset,
            });
        }

        let mut components = Vec::new();
        let mut component_start = 0;
        let mut escaped = false;
        let mut source_components = 0usize;
        for (relative, character) in content.char_indices() {
            if escaped {
                escaped = false;
                continue;
            }
            if character == '\\' {
                escaped = true;
            } else if character == '.' {
                if relative == component_start {
                    return Err(KqlError::EmptyPathComponent {
                        offset: content_offset + relative,
                    });
                }
                self.push_path_component(
                    &mut components,
                    &content[component_start..relative],
                    content_offset + component_start,
                    &mut source_components,
                )?;
                component_start = relative + 1;
            }
        }
        if component_start == content.len() {
            return Err(KqlError::EmptyPathComponent {
                offset: content_offset + component_start,
            });
        }
        self.push_path_component(
            &mut components,
            &content[component_start..],
            content_offset + component_start,
            &mut source_components,
        )?;
        Ok(ColumnPath {
            namespace,
            components,
        })
    }

    fn push_path_component(
        &mut self,
        components: &mut Vec<PathComponent>,
        raw: &str,
        offset: usize,
        source_components: &mut usize,
    ) -> Result<(), KqlError> {
        *source_components = source_components
            .checked_add(1)
            .ok_or(KqlError::SizeOverflow { offset })?;
        if *source_components > self.limits.max_path_components() {
            return Err(KqlError::PathLimitExceeded {
                offset,
                limit: self.limits.max_path_components(),
            });
        }
        let encoded = self.decode_kql(raw, offset, EscapeContext::Key)?;
        let wildcard = encoded == "*";
        let value = self.decode_descriptor_component(&encoded, offset)?;
        if wildcard && components.last().is_some_and(PathComponent::is_wildcard) {
            return Ok(());
        }
        reserve_one(components, offset, KqlResource::PathComponents)?;
        components.push(PathComponent { value, wildcard });
        Ok(())
    }

    fn finish_path(&mut self, path: ColumnPath, offset: usize) -> Result<ColumnPath, KqlError> {
        if self.nested_depth > 0 && path.namespace != ColumnNamespace::Default {
            return Err(KqlError::NestedNamespace { offset });
        }
        let component_count = self
            .active_prefix
            .len()
            .checked_add(path.components.len())
            .ok_or(KqlError::SizeOverflow { offset })?;
        if component_count > self.limits.max_path_components() {
            return Err(KqlError::PathLimitExceeded {
                offset,
                limit: self.limits.max_path_components(),
            });
        }

        let mut components = Vec::new();
        components
            .try_reserve_exact(component_count)
            .map_err(|_| KqlError::AllocationFailed {
                offset,
                resource: KqlResource::PathComponents,
                requested: component_count,
            })?;
        let prefix_string_bytes =
            self.active_prefix
                .iter()
                .try_fold(0usize, |total, component| {
                    total
                        .checked_add(component.value.len())
                        .ok_or(KqlError::SizeOverflow { offset })
                })?;
        self.builder
            .claim_string_bytes(prefix_string_bytes, offset)?;
        for component in &self.active_prefix {
            let mut value = String::new();
            value
                .try_reserve_exact(component.value.len())
                .map_err(|_| KqlError::AllocationFailed {
                    offset,
                    resource: KqlResource::Strings,
                    requested: component.value.len(),
                })?;
            value.push_str(&component.value);
            components.push(PathComponent {
                value,
                wildcard: component.wildcard,
            });
        }
        for component in path.components {
            if component.wildcard && components.last().is_some_and(PathComponent::is_wildcard) {
                continue;
            }
            components.push(component);
        }
        let namespace = if self.nested_depth > 0 {
            self.active_namespace
        } else {
            path.namespace
        };
        Ok(ColumnPath {
            namespace,
            components,
        })
    }

    fn default_wildcard_path(&mut self, offset: usize) -> Result<ColumnPath, KqlError> {
        let value = self.copy_owned("*", offset)?;
        let mut components = Vec::new();
        reserve_one(&mut components, offset, KqlResource::PathComponents)?;
        components.push(PathComponent {
            value,
            wildcard: true,
        });
        let local = ColumnPath {
            namespace: ColumnNamespace::Default,
            components,
        };
        self.finish_path(local, offset)
    }

    fn enter_nested(
        &mut self,
        path: ColumnPath,
        expression_start: usize,
        brace_offset: usize,
    ) -> Result<(), KqlError> {
        if self.nested_depth > 0 && path.namespace != ColumnNamespace::Default {
            return Err(KqlError::NestedNamespace {
                offset: expression_start,
            });
        }
        let previous_prefix_len = self.active_prefix.len();
        let previous_namespace = self.active_namespace;
        let combined = previous_prefix_len
            .checked_add(path.components.len())
            .ok_or(KqlError::SizeOverflow {
                offset: expression_start,
            })?;
        if combined > self.limits.max_path_components() {
            return Err(KqlError::PathLimitExceeded {
                offset: expression_start,
                limit: self.limits.max_path_components(),
            });
        }
        self.active_prefix
            .try_reserve(path.components.len())
            .map_err(|_| KqlError::AllocationFailed {
                offset: expression_start,
                resource: KqlResource::PathComponents,
                requested: path.components.len(),
            })?;
        for component in path.components {
            if component.wildcard
                && self
                    .active_prefix
                    .last()
                    .is_some_and(PathComponent::is_wildcard)
            {
                continue;
            }
            self.active_prefix.push(component);
        }
        if self.nested_depth == 0 {
            self.active_namespace = path.namespace;
        }
        self.nested_depth = self
            .nested_depth
            .checked_add(1)
            .ok_or(KqlError::SizeOverflow {
                offset: expression_start,
            })?;
        self.open_group(
            GroupKind::Nested {
                previous_prefix_len,
                previous_namespace,
            },
            expression_start,
        )?;
        if brace_offset < expression_start {
            return Err(KqlError::MalformedExpression {
                offset: brace_offset,
            });
        }
        Ok(())
    }

    fn open_group(&mut self, kind: GroupKind, offset: usize) -> Result<(), KqlError> {
        self.syntax_depth = self
            .syntax_depth
            .checked_add(1)
            .ok_or(KqlError::SizeOverflow { offset })?;
        if self.syntax_depth > self.limits.max_depth() {
            return Err(KqlError::DepthLimitExceeded {
                offset,
                limit: self.limits.max_depth(),
            });
        }
        self.push_operator(PendingOperator::Group(GroupMarker {
            kind,
            values_before: self.values.len(),
            offset,
        }))
    }

    fn close_group(&mut self, closing: KqlToken) -> Result<(), KqlError> {
        self.reduce_binary_until_group(closing.span.start())?;
        let marker = match self.operators.pop() {
            Some(PendingOperator::Group(marker)) => marker,
            Some(other) => {
                self.operators.push(other);
                return Err(self.unexpected(KqlExpected::EndOfInput, Some(closing)));
            }
            None => {
                return Err(self.unexpected(KqlExpected::EndOfInput, Some(closing)));
            }
        };
        let expected_kind = match marker.kind {
            GroupKind::Parenthesis => KqlTokenKind::RightParenthesis,
            GroupKind::Nested { .. } => KqlTokenKind::RightBrace,
        };
        if closing.kind != expected_kind {
            let expected = if expected_kind == KqlTokenKind::RightParenthesis {
                KqlExpected::ClosingParenthesis
            } else {
                KqlExpected::ClosingBrace
            };
            return Err(self.unexpected(expected, Some(closing)));
        }
        if self.values.len() != marker.values_before + 1 {
            return Err(KqlError::MalformedExpression {
                offset: closing.span.start(),
            });
        }
        let node = *self
            .values
            .last()
            .ok_or_else(|| KqlError::MalformedExpression {
                offset: closing.span.start(),
            })?;
        self.builder
            .extend_span(node, marker.offset, closing.span.end())?;
        if let GroupKind::Nested {
            previous_prefix_len,
            previous_namespace,
        } = marker.kind
        {
            self.active_prefix.truncate(previous_prefix_len);
            self.active_namespace = previous_namespace;
            self.nested_depth =
                self.nested_depth
                    .checked_sub(1)
                    .ok_or_else(|| KqlError::MalformedExpression {
                        offset: closing.span.start(),
                    })?;
        }
        self.syntax_depth =
            self.syntax_depth
                .checked_sub(1)
                .ok_or_else(|| KqlError::MalformedExpression {
                    offset: closing.span.start(),
                })?;
        self.reduce_unary()
    }

    fn reduce_unary(&mut self) -> Result<(), KqlError> {
        while let Some(PendingOperator::Not { offset }) = self.operators.last().copied() {
            self.operators.pop();
            let operand = self
                .values
                .pop()
                .ok_or(KqlError::MalformedExpression { offset })?;
            let node = self.builder.push_not(operand, offset)?;
            self.push_value(node, offset)?;
        }
        Ok(())
    }

    fn reduce_binary_until_group(&mut self, offset: usize) -> Result<(), KqlError> {
        loop {
            match self.operators.last().copied() {
                Some(PendingOperator::Binary { operator, offset }) => {
                    self.operators.pop();
                    self.reduce_binary(operator, offset)?;
                }
                Some(PendingOperator::Not { .. }) => self.reduce_unary()?,
                Some(PendingOperator::Group(_)) | None => return Ok(()),
            }
            if self.values.is_empty() {
                return Err(KqlError::MalformedExpression { offset });
            }
        }
    }

    fn reduce_binary(&mut self, operator: BooleanOperator, offset: usize) -> Result<(), KqlError> {
        let right = self
            .values
            .pop()
            .ok_or(KqlError::MalformedExpression { offset })?;
        let left = self
            .values
            .pop()
            .ok_or(KqlError::MalformedExpression { offset })?;
        let node = self.builder.push_boolean(operator, left, right, offset)?;
        self.push_value(node, offset)
    }

    fn finish_operators(&mut self) -> Result<(), KqlError> {
        while let Some(operator) = self.operators.pop() {
            match operator {
                PendingOperator::Binary { operator, offset } => {
                    self.reduce_binary(operator, offset)?;
                }
                PendingOperator::Not { offset } => {
                    let operand = self
                        .values
                        .pop()
                        .ok_or(KqlError::MalformedExpression { offset })?;
                    let node = self.builder.push_not(operand, offset)?;
                    self.push_value(node, offset)?;
                }
                PendingOperator::Group(marker) => {
                    let expected = match marker.kind {
                        GroupKind::Parenthesis => KqlExpected::ClosingParenthesis,
                        GroupKind::Nested { .. } => KqlExpected::ClosingBrace,
                    };
                    return Err(self.unexpected_at(self.input.len(), expected, None));
                }
            }
        }
        Ok(())
    }

    fn push_operator(&mut self, operator: PendingOperator) -> Result<(), KqlError> {
        let offset = operator.offset();
        reserve_one(&mut self.operators, offset, KqlResource::Operators)?;
        self.operators.push(operator);
        Ok(())
    }

    fn push_value(&mut self, node: NodeId, offset: usize) -> Result<(), KqlError> {
        reserve_one(&mut self.values, offset, KqlResource::Values)?;
        self.values.push(node);
        Ok(())
    }

    fn literal_content(&self, token: KqlToken) -> Result<(&'input str, usize), KqlError> {
        let KqlTokenKind::Literal { quoted } = token.kind else {
            return Err(self.unexpected_at(token.span.start(), KqlExpected::Literal, Some(token)));
        };
        let start = token.span.start() + usize::from(quoted);
        let end = token
            .span
            .end()
            .checked_sub(usize::from(quoted))
            .ok_or_else(|| KqlError::MalformedExpression {
                offset: token.span.start(),
            })?;
        let content = self
            .input
            .get(start..end)
            .ok_or(KqlError::MalformedExpression { offset: start })?;
        Ok((content, start))
    }

    fn copy_quoted_raw(&mut self, token: KqlToken) -> Result<String, KqlError> {
        let (raw, offset) = self.literal_content(token)?;
        self.copy_owned(raw, offset)
    }

    fn decode_kql(
        &mut self,
        raw: &str,
        base_offset: usize,
        context: EscapeContext,
    ) -> Result<String, KqlError> {
        let mut decoded = self.string_with_capacity(raw.len(), base_offset)?;
        let mut cursor = 0;
        while cursor < raw.len() {
            let character = raw[cursor..]
                .chars()
                .next()
                .ok_or(KqlError::MalformedExpression {
                    offset: base_offset + cursor,
                })?;
            if character != '\\' {
                decoded.push(character);
                cursor += character.len_utf8();
                continue;
            }

            let slash_offset = base_offset + cursor;
            cursor += 1;
            let escaped = *raw.as_bytes().get(cursor).ok_or(KqlError::InvalidEscape {
                offset: slash_offset,
            })?;
            cursor += 1;
            match escaped {
                b'\\' => decoded.push_str("\\\\"),
                b'"' => decoded.push('"'),
                b't' => decoded.push('\t'),
                b'r' => decoded.push('\r'),
                b'n' => decoded.push('\n'),
                b'b' => decoded.push('\u{0008}'),
                b'f' => decoded.push('\u{000c}'),
                b'u' => {
                    let end = cursor.checked_add(4).ok_or(KqlError::SizeOverflow {
                        offset: slash_offset,
                    })?;
                    let hex = raw.get(cursor..end).ok_or(KqlError::InvalidUnicodeEscape {
                        offset: slash_offset,
                    })?;
                    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                        return Err(KqlError::InvalidUnicodeEscape {
                            offset: slash_offset,
                        });
                    }
                    let scalar = u32::from_str_radix(hex, 16).map_err(|_| {
                        KqlError::InvalidUnicodeEscape {
                            offset: slash_offset,
                        }
                    })?;
                    let character =
                        char::from_u32(scalar).ok_or(KqlError::InvalidUnicodeEscape {
                            offset: slash_offset,
                        })?;
                    cursor = end;
                    append_unicode_literal(character, context, &mut decoded);
                }
                b'{' => decoded.push('{'),
                b'}' => decoded.push('}'),
                b'(' => decoded.push('('),
                b')' => decoded.push(')'),
                b'<' => decoded.push('<'),
                b'>' => decoded.push('>'),
                b'*' => decoded.push_str("\\*"),
                b'?' if context == EscapeContext::Value => decoded.push_str("\\?"),
                b'?' => decoded.push('?'),
                b'@' => decoded.push('@'),
                b'$' => decoded.push('$'),
                b'!' => decoded.push('!'),
                b'#' => decoded.push('#'),
                b'.' if context == EscapeContext::Key => decoded.push('.'),
                _ => {
                    return Err(KqlError::InvalidEscape {
                        offset: slash_offset,
                    });
                }
            }
        }
        Ok(decoded)
    }

    fn decode_descriptor_component(
        &mut self,
        encoded: &str,
        offset: usize,
    ) -> Result<String, KqlError> {
        let mut decoded = self.string_with_capacity(encoded.len(), offset)?;
        let mut escaped = false;
        for character in encoded.chars() {
            if escaped {
                decoded.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else {
                decoded.push(character);
            }
        }
        if escaped {
            return Err(KqlError::InvalidEscape {
                offset: offset + encoded.len().saturating_sub(1),
            });
        }
        Ok(decoded)
    }

    fn clean_wildcards(&mut self, decoded: &str, offset: usize) -> Result<String, KqlError> {
        let mut cleaned = self.string_with_capacity(decoded.len(), offset)?;
        let mut chars = decoded.chars().peekable();
        let mut escaped = false;
        while let Some(character) = chars.next() {
            if escaped {
                if matches!(character, '*' | '?' | '\\') {
                    cleaned.push('\\');
                }
                cleaned.push(character);
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '*' {
                cleaned.push('*');
                while chars.next_if_eq(&'*').is_some() {}
            } else {
                cleaned.push(character);
            }
        }
        Ok(cleaned)
    }

    fn copy_owned(&mut self, value: &str, offset: usize) -> Result<String, KqlError> {
        let mut copy = self.string_with_capacity(value.len(), offset)?;
        copy.push_str(value);
        Ok(copy)
    }

    fn string_with_capacity(&mut self, capacity: usize, offset: usize) -> Result<String, KqlError> {
        self.builder.claim_string_bytes(capacity, offset)?;
        let mut value = String::new();
        value
            .try_reserve_exact(capacity)
            .map_err(|_| KqlError::AllocationFailed {
                offset,
                resource: KqlResource::Strings,
                requested: capacity,
            })?;
        Ok(value)
    }

    fn current(&self) -> Option<KqlToken> {
        self.tokens.get(self.cursor).copied()
    }

    fn unexpected(&self, expected: KqlExpected, found: Option<KqlToken>) -> KqlError {
        let offset = found.map_or(self.input.len(), |token| token.span.start());
        self.unexpected_at(offset, expected, found)
    }

    fn unexpected_at(
        &self,
        offset: usize,
        expected: KqlExpected,
        found: Option<KqlToken>,
    ) -> KqlError {
        KqlError::UnexpectedToken {
            offset: offset.min(self.input.len()),
            expected,
            found: found.map(|token| token.kind),
        }
    }
}

struct AstBuilder {
    nodes: Vec<ExpressionNode>,
    depths: Vec<usize>,
    limits: KqlLimits,
    string_bytes: usize,
}

impl AstBuilder {
    const fn new(limits: KqlLimits) -> Self {
        Self {
            nodes: Vec::new(),
            depths: Vec::new(),
            limits,
            string_bytes: 0,
        }
    }

    fn push_leaf(&mut self, kind: ExpressionKind, span: SourceSpan) -> Result<NodeId, KqlError> {
        self.push(kind, span, 1)
    }

    fn push_not(&mut self, operand: NodeId, offset: usize) -> Result<NodeId, KqlError> {
        let operand_depth = *self
            .depths
            .get(operand.index())
            .ok_or(KqlError::MalformedExpression { offset })?;
        let depth = operand_depth
            .checked_add(1)
            .ok_or(KqlError::SizeOverflow { offset })?;
        let operand_span = self
            .nodes
            .get(operand.index())
            .ok_or(KqlError::MalformedExpression { offset })?
            .span;
        self.push(
            ExpressionKind::Not { operand },
            SourceSpan::new(offset, operand_span.end()),
            depth,
        )
    }

    fn push_boolean(
        &mut self,
        operator: BooleanOperator,
        left: NodeId,
        right: NodeId,
        offset: usize,
    ) -> Result<NodeId, KqlError> {
        let left_depth = *self
            .depths
            .get(left.index())
            .ok_or(KqlError::MalformedExpression { offset })?;
        let right_depth = *self
            .depths
            .get(right.index())
            .ok_or(KqlError::MalformedExpression { offset })?;
        let depth = left_depth
            .max(right_depth)
            .checked_add(1)
            .ok_or(KqlError::SizeOverflow { offset })?;
        let left_span = self
            .nodes
            .get(left.index())
            .ok_or(KqlError::MalformedExpression { offset })?
            .span;
        let right_span = self
            .nodes
            .get(right.index())
            .ok_or(KqlError::MalformedExpression { offset })?
            .span;
        self.push(
            ExpressionKind::Boolean {
                operator,
                left,
                right,
            },
            SourceSpan::new(left_span.start(), right_span.end()),
            depth,
        )
    }

    fn push(
        &mut self,
        kind: ExpressionKind,
        span: SourceSpan,
        depth: usize,
    ) -> Result<NodeId, KqlError> {
        if depth > self.limits.max_depth() {
            return Err(KqlError::DepthLimitExceeded {
                offset: span.start(),
                limit: self.limits.max_depth(),
            });
        }
        if self.nodes.len() >= self.limits.max_nodes() {
            return Err(KqlError::NodeLimitExceeded {
                offset: span.start(),
                limit: self.limits.max_nodes(),
            });
        }
        reserve_one(&mut self.nodes, span.start(), KqlResource::Nodes)?;
        reserve_one(&mut self.depths, span.start(), KqlResource::Nodes)?;
        let id = NodeId::new(self.nodes.len());
        self.nodes.push(ExpressionNode { kind, span });
        self.depths.push(depth);
        Ok(id)
    }

    fn extend_span(&mut self, node: NodeId, start: usize, end: usize) -> Result<(), KqlError> {
        let expression = self
            .nodes
            .get_mut(node.index())
            .ok_or(KqlError::MalformedExpression { offset: start })?;
        expression.span = SourceSpan::new(start, end);
        Ok(())
    }

    fn claim_string_bytes(&mut self, bytes: usize, offset: usize) -> Result<(), KqlError> {
        let required = self
            .string_bytes
            .checked_add(bytes)
            .ok_or(KqlError::SizeOverflow { offset })?;
        if required > self.limits.max_owned_string_bytes() {
            return Err(KqlError::StringLimitExceeded {
                offset,
                required,
                limit: self.limits.max_owned_string_bytes(),
            });
        }
        self.string_bytes = required;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug)]
enum PendingOperator {
    Binary {
        operator: BooleanOperator,
        offset: usize,
    },
    Not {
        offset: usize,
    },
    Group(GroupMarker),
}

impl PendingOperator {
    const fn offset(self) -> usize {
        match self {
            Self::Binary { offset, .. } | Self::Not { offset } => offset,
            Self::Group(marker) => marker.offset,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct GroupMarker {
    kind: GroupKind,
    values_before: usize,
    offset: usize,
}

#[derive(Clone, Copy, Debug)]
enum GroupKind {
    Parenthesis,
    Nested {
        previous_prefix_len: usize,
        previous_namespace: ColumnNamespace,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EscapeContext {
    Key,
    Value,
}

const fn is_column_operator(kind: KqlTokenKind) -> bool {
    matches!(
        kind,
        KqlTokenKind::Colon
            | KqlTokenKind::Less
            | KqlTokenKind::LessOrEqual
            | KqlTokenKind::Greater
            | KqlTokenKind::GreaterOrEqual
    )
}

fn append_unicode_literal(character: char, context: EscapeContext, output: &mut String) {
    match character {
        '\\' => output.push_str("\\\\"),
        '?' if context == EscapeContext::Value => output.push_str("\\?"),
        '*' => output.push_str("\\*"),
        _ => output.push(character),
    }
}

fn reserve_one<T>(
    values: &mut Vec<T>,
    offset: usize,
    resource: KqlResource,
) -> Result<(), KqlError> {
    if values.len() == values.capacity() {
        values
            .try_reserve(1)
            .map_err(|_| KqlError::AllocationFailed {
                offset,
                resource,
                requested: 1,
            })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root_kind(query: &ParsedQuery) -> &ExpressionKind {
        query
            .node(query.root())
            .expect("root belongs to query")
            .kind()
    }

    fn predicate(query: &ParsedQuery, id: NodeId) -> &Predicate {
        match query.node(id).expect("node belongs to query").kind() {
            ExpressionKind::Predicate(predicate) => predicate,
            other => panic!("expected predicate, got {other:?}"),
        }
    }

    #[test]
    fn ports_cpp_basic_default_wildcard_and_case_insensitive_operators() {
        for source in [
            "value",
            "  value ",
            "\"value\"",
            "*:value",
            " \"*\" : \"value\" ",
        ] {
            let query = parse_kql(source, KqlLimits::default()).expect("C++ vector parses");
            let ExpressionKind::Predicate(predicate) = root_kind(&query) else {
                panic!("expected predicate");
            };
            assert!(predicate.path().is_default_wildcard());
            assert_eq!(
                Literal::String(StringLiteral {
                    wildcard_pattern: "value".to_owned()
                }),
                *predicate.value()
            );
        }

        let query = parse_kql("a:1 aNd b:2", KqlLimits::default()).expect("mixed-case AND");
        assert!(matches!(
            root_kind(&query),
            ExpressionKind::Boolean {
                operator: BooleanOperator::And,
                ..
            }
        ));
    }

    #[test]
    fn and_or_are_equal_precedence_left_associative_and_not_is_tighter() {
        let query = parse_kql("NOT a:1 OR b:2 AND c:3", KqlLimits::default())
            .expect("compatibility trap parses");
        let ExpressionKind::Boolean {
            operator: BooleanOperator::And,
            left,
            right,
        } = root_kind(&query)
        else {
            panic!("root should be final left-associated AND");
        };
        assert_eq!(
            "c",
            predicate(&query, *right).path().components()[0].value()
        );
        let ExpressionKind::Boolean {
            operator: BooleanOperator::Or,
            left: not_a,
            right: b,
        } = query.node(*left).expect("left node").kind()
        else {
            panic!("left should be OR");
        };
        assert!(matches!(
            query.node(*not_a).expect("NOT node").kind(),
            ExpressionKind::Not { .. }
        ));
        assert_eq!("b", predicate(&query, *b).path().components()[0].value());
    }

    #[test]
    fn quoted_values_keep_cpp_type_inference_order() {
        let vectors = [
            ("\"-9223372036854775808\"", "integer"),
            ("\"1.25e2\"", "float"),
            ("\"true\"", "boolean"),
            ("\"null\"", "null"),
            ("\"TRUE\"", "string"),
            ("\"1e9999\"", "string"),
        ];
        for (source, expected) in vectors {
            let query = parse_kql(source, KqlLimits::default()).expect("typed literal");
            let ExpressionKind::Predicate(predicate) = root_kind(&query) else {
                panic!("expected predicate");
            };
            let actual = match predicate.value() {
                Literal::Integer { .. } => "integer",
                Literal::Float { .. } => "float",
                Literal::Boolean(_) => "boolean",
                Literal::Null => "null",
                Literal::String(_) => "string",
                Literal::Timestamp(_) => "timestamp",
            };
            assert_eq!(expected, actual, "{source}");
        }
    }

    #[test]
    fn nested_paths_namespaces_and_literal_wildcards_match_cpp() {
        let query = parse_kql(r"@a\.b:{c.*:{\*:\u003F} AND d:2}", KqlLimits::default())
            .expect("nested path");
        let ExpressionKind::Boolean { left, right, .. } = root_kind(&query) else {
            panic!("nested query should contain AND");
        };
        let left = predicate(&query, *left);
        assert_eq!(ColumnNamespace::Autogenerated, left.path().namespace());
        assert_eq!(
            ["a.b", "c", "*", "*"],
            left.path()
                .components()
                .iter()
                .map(PathComponent::value)
                .collect::<Vec<_>>()
                .as_slice()
        );
        assert!(left.path().components()[2].is_wildcard());
        assert!(!left.path().components()[3].is_wildcard());
        assert_eq!(
            "d",
            predicate(&query, *right).path().components()[1].value()
        );

        assert!(matches!(
            parse_kql("a:{@b:1}", KqlLimits::default()),
            Err(KqlError::NestedNamespace { .. })
        ));
    }

    #[test]
    fn value_escaping_and_wildcard_cleanup_port_cpp_vectors() {
        let query = parse_kql(r#"*: "***t**\*s\?t?**\u005C\u002A""#, KqlLimits::default())
            .expect("escape vector");
        let ExpressionKind::Predicate(predicate) = root_kind(&query) else {
            panic!("predicate");
        };
        let Literal::String(value) = predicate.value() else {
            panic!("string");
        };
        assert_eq!(r"*t*\*s\?t?*\\\*", value.wildcard_pattern());
        assert!(value.has_wildcards());

        let vectors = [
            (r"\\", r"\\"),
            (r"\??", r"\??"),
            (r"\**", r"\**"),
            (r"\u9999", "香"),
            (r"\r\n\t\b\f", "\r\n\t\u{0008}\u{000c}"),
            (r#"\""#, "\""),
            (r"\{\}\(\)\<\>", "{}()<>"),
            (r"\u003F", r"\?"),
            (r"\u002A", r"\*"),
            (r"\u005C", r"\\"),
        ];
        for (source, expected) in vectors {
            let input = format!(r#"*: "{source}""#);
            let query = parse_kql(&input, KqlLimits::default()).expect("C++ escape vector");
            let ExpressionKind::Predicate(predicate) = root_kind(&query) else {
                panic!("predicate");
            };
            let Literal::String(value) = predicate.value() else {
                panic!("string");
            };
            assert_eq!(expected, value.wildcard_pattern(), "{source}");
        }
    }

    #[test]
    fn compact_lists_retain_modes_and_typed_values_without_dnf_expansion() {
        for (source, expected, len) in [
            ("key:()", ListOperator::Any, 0),
            ("key:(one two)", ListOperator::Any, 2),
            ("key:(OR one two)", ListOperator::Any, 2),
            ("key:(AND 1 2)", ListOperator::All, 2),
            ("key:(NOT true null)", ListOperator::None, 2),
        ] {
            let query = parse_kql(source, KqlLimits::default()).expect("list parses");
            let ExpressionKind::List(list) = root_kind(&query) else {
                panic!("list expression");
            };
            assert_eq!(expected, list.operator());
            assert_eq!(len, list.values().len());
            assert_eq!("key", list.path().components()[0].value());
            assert_eq!(1, query.nodes().len());
        }
    }

    #[test]
    fn timestamp_is_exact_lowercase_adjacent_and_structurally_distinct() {
        let query = parse_kql(
            r#"* < timestamp("1970-01-01 00:00:00.000000001", "\N")"#,
            KqlLimits::default(),
        )
        .expect("timestamp expression");
        let ExpressionKind::Predicate(predicate) = root_kind(&query) else {
            panic!("predicate");
        };
        let Literal::Timestamp(timestamp) = predicate.value() else {
            panic!("timestamp literal");
        };
        assert_eq!("1970-01-01 00:00:00.000000001", timestamp.value());
        assert_eq!(Some(r"\N"), timestamp.pattern());

        let unresolved = parse_kql(
            r#"a: timestamp("semantic resolution is deferred")"#,
            KqlLimits::default(),
        )
        .expect("parser retains a syntactically valid unresolved timestamp");
        assert!(matches!(
            root_kind(&unresolved),
            ExpressionKind::Predicate(Predicate {
                value: Literal::Timestamp(_),
                ..
            })
        ));

        for invalid in [
            r#"a: Timestamp("1")"#,
            r#"a: timestamp ("1")"#,
            "a: timestamp()",
            "a: timestamp(1)",
            r#"a: timestamp("1",)"#,
        ] {
            assert!(
                parse_kql(invalid, KqlLimits::default()).is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn limits_and_error_offsets_cover_hostile_shapes() {
        assert!(matches!(
            parse_kql("a:b", KqlLimits::new(3, 3, 0, 4, 4, 4, 16)),
            Err(KqlError::NodeLimitExceeded {
                offset: 0,
                limit: 0
            })
        ));
        assert!(matches!(
            parse_kql("a.b:c", KqlLimits::new(5, 3, 2, 4, 1, 4, 32)),
            Err(KqlError::PathLimitExceeded {
                offset: 2,
                limit: 1
            })
        ));
        assert!(matches!(
            parse_kql("a:(b c)", KqlLimits::new(7, 6, 2, 4, 2, 1, 32)),
            Err(KqlError::ListLimitExceeded {
                offset: 5,
                limit: 1
            })
        ));
        assert!(matches!(
            parse_kql("((a:b))", KqlLimits::new(7, 7, 2, 1, 2, 2, 32)),
            Err(KqlError::DepthLimitExceeded {
                offset: 1,
                limit: 1
            })
        ));
        assert_eq!(
            2,
            parse_kql(r"a:\q", KqlLimits::default())
                .expect_err("invalid escape")
                .offset()
        );
    }

    #[test]
    fn malformed_cpp_vectors_are_rejected_without_recursive_parsing() {
        for invalid in [
            "",
            "NOT :",
            "NOT key:",
            "a:a AND",
            ":a",
            ".a:*",
            "a..b:*",
            "a.:*",
            "a:(one OR two)",
            "a:{b:1",
            "(a:1}",
        ] {
            assert!(
                parse_kql(invalid, KqlLimits::default()).is_err(),
                "{invalid}"
            );
        }

        let nested = format!("{}a:b{}", "(".repeat(200), ")".repeat(200));
        let query = parse_kql(&nested, KqlLimits::default()).expect("bounded iterative groups");
        assert_eq!(1, query.nodes().len());
    }
}
