/*
 * Minimal SQL grammar for CLP-S search.
 *
 * Supports:
 *   SELECT [columns | * | CLP_GET_* functions]
 *   [FROM <table>]
 *   [WHERE <conditions>]  (comparisons, AND/OR/NOT, LIKE, IN, BETWEEN, IS NULL)
 *   [LIMIT <n>]
 *
 * Aggregate functions:
 *   COUNT(*)        - count matching records
 *   MIN(col)        - minimum value of a column
 *   MAX(col)        - maximum value of a column
 *   SUM(col)        - sum of column values
 *   AVG(col)        - average of column values
 *   ARBITRARY(col)  - any value from matching records (Presto-style)
 *   ANY_VALUE(col)  - any value from matching records (MySQL/BigQuery-style)
 *   Supports AS alias: COUNT(*) AS cnt
 *
 * Timestamp literals:
 *   TIMESTAMP '<datetime>'   - Presto-style timestamp literal (e.g. '2024-01-15 10:30:00')
 *   DATE '<date>'            - date-only shorthand, implies 00:00:00
 *
 * CLP-specific functions:
 *   CLP_GET_INT(<field>)         - extract field as integer (type-filtered)
 *   CLP_GET_FLOAT(<field>)       - extract field as float (type-filtered)
 *   CLP_GET_STRING(<field>)      - extract field as string (type-filtered)
 *   CLP_GET_BOOL(<field>)        - extract field as boolean (type-filtered)
 *   CLP_GET_JSON_STRING(<field>) - extract field as JSON (any type)
 *   CLP_GET_JSON_STRING()        - dump entire record as JSON
 *   CLP_WILDCARD_COLUMN()        - match any column (equivalent to KQL's *)
 */

grammar Sql;

// ── Parser Rules ──────────────────────────────────────────────────────────────

singleStatement
    : statement EOF
    ;

statement
    : query    #statementDefault
    ;

query
    : querySpecification (LIMIT limit=INTEGER_VALUE)?
    ;

querySpecification
    : SELECT setQuantifier? selectItem (',' selectItem)*
      (FROM tableName=qualifiedName)?
      (WHERE where=booleanExpression)?
    ;

setQuantifier
    : DISTINCT
    | ALL
    ;

selectItem
    : expression (AS? identifier)?   #selectSingle
    | ASTERISK                       #selectAll
    ;

expression
    : booleanExpression
    ;

booleanExpression
    : valueExpression predicate[$valueExpression.ctx]?              #predicated
    | NOT booleanExpression                                         #logicalNot
    | left=booleanExpression operator=AND right=booleanExpression   #logicalBinary
    | left=booleanExpression operator=OR right=booleanExpression    #logicalBinary
    ;

// Workaround for https://github.com/antlr/antlr4/issues/780
predicate[antlr4::ParserRuleContext* value]
    : comparisonOperator right=valueExpression                             #comparison
    | NOT? BETWEEN lower=valueExpression AND upper=valueExpression         #between
    | NOT? IN '(' expression (',' expression)* ')'                         #inList
    | NOT? LIKE pattern=valueExpression                                   #like
    | IS NOT? NULL                                                         #nullPredicate
    ;

valueExpression
    : primaryExpression                                                                  #valueExpressionDefault
    | operator=(MINUS | PLUS) valueExpression                                            #arithmeticUnary
    | left=valueExpression operator=(ASTERISK | SLASH | PERCENT) right=valueExpression   #arithmeticBinary
    | left=valueExpression operator=(PLUS | MINUS) right=valueExpression                 #arithmeticBinary
    ;

primaryExpression
    : NULL                                             #nullLiteral
    | number                                           #numericLiteral
    | booleanValue                                     #booleanLiteral
    | string                                           #stringLiteral
    | TIMESTAMP string                                 #timestampLiteral
    | DATE string                                      #dateLiteral
    | CLP_GET_INT '(' expression ')'                   #clpGetInt
    | CLP_GET_FLOAT '(' expression ')'                 #clpGetFloat
    | CLP_GET_STRING '(' expression ')'                #clpGetString
    | CLP_GET_BOOL '(' expression ')'                  #clpGetBool
    | CLP_GET_JSON_STRING '(' expression? ')'          #clpGetJsonString
    | CLP_WILDCARD_COLUMN '(' ')'                      #clpWildcardColumn
    | COUNT '(' ASTERISK ')'                           #aggCountStar
    | COUNT '(' expression ')'                         #aggCount
    | MIN '(' expression ')'                           #aggMin
    | MAX '(' expression ')'                           #aggMax
    | SUM '(' expression ')'                           #aggSum
    | AVG '(' expression ')'                           #aggAvg
    | ARBITRARY '(' expression ')'                     #aggArbitrary
    | ANY_VALUE '(' expression ')'                     #aggArbitrary
    | identifier                                       #columnReference
    | base=primaryExpression '.' fieldName=identifier   #dereference
    | '(' expression ')'                               #parenthesizedExpression
    ;

qualifiedName
    : identifier ('.' identifier)*
    ;

identifier
    : IDENTIFIER              #unquotedIdentifier
    | QUOTED_IDENTIFIER       #quotedIdentifier
    | BACKQUOTED_IDENTIFIER   #backQuotedIdentifier
    | nonReserved             #unquotedIdentifier
    ;

number
    : DECIMAL_VALUE    #decimalLiteral
    | DOUBLE_VALUE     #doubleLiteral
    | INTEGER_VALUE    #integerLiteral
    ;

string
    : STRING           #basicStringLiteral
    ;

booleanValue
    : TRUE
    | FALSE
    ;

comparisonOperator
    : EQ | NEQ | LT | LTE | GT | GTE
    ;

// Keywords that can also be used as identifiers (e.g., column names)
nonReserved
    : ALL | ANY_VALUE | ARBITRARY | AS | AVG | COUNT | DATE | DISTINCT | LIMIT | MAX | MIN | SUM
    | TIMESTAMP
    ;

// ── Lexer Rules ───────────────────────────────────────────────────────────────

// CLP-specific function keywords (must precede IDENTIFIER)
CLP_GET_INT:         'CLP_GET_INT';
CLP_GET_FLOAT:       'CLP_GET_FLOAT';
CLP_GET_STRING:      'CLP_GET_STRING';
CLP_GET_BOOL:        'CLP_GET_BOOL';
CLP_GET_JSON_STRING: 'CLP_GET_JSON_STRING';
CLP_WILDCARD_COLUMN: 'CLP_WILDCARD_COLUMN';

// SQL keywords
// NOTE: These must be kept in sync with cSqlKeywords in sql.cpp's normalize_sql_keywords().
ALL:       'ALL';
AND:       'AND';
ANY_VALUE: 'ANY_VALUE';
ARBITRARY: 'ARBITRARY';
AS:        'AS';
AVG:       'AVG';
BETWEEN:   'BETWEEN';
COUNT:     'COUNT';
DATE:      'DATE';
DISTINCT:  'DISTINCT';
FALSE:     'FALSE';
FROM:      'FROM';
IN:        'IN';
IS:        'IS';
LIKE:      'LIKE';
LIMIT:     'LIMIT';
MAX:       'MAX';
MIN:       'MIN';
NOT:       'NOT';
NULL:      'NULL';
OR:        'OR';
SELECT:    'SELECT';
SUM:       'SUM';
TIMESTAMP: 'TIMESTAMP';
TRUE:      'TRUE';
WHERE:     'WHERE';

// Operators
EQ:       '=';
NEQ:      '<>' | '!=';
LT:       '<';
LTE:      '<=';
GT:       '>';
GTE:      '>=';

PLUS:     '+';
MINUS:    '-';
ASTERISK: '*';
SLASH:    '/';
PERCENT:  '%';

// Literals
STRING
    : '\'' ( ~'\'' | '\'\'' )* '\''
    ;

INTEGER_VALUE
    : DIGIT+
    ;

DECIMAL_VALUE
    : DIGIT+ '.' DIGIT*
    | '.' DIGIT+
    ;

DOUBLE_VALUE
    : DIGIT+ ('.' DIGIT*)? EXPONENT
    | '.' DIGIT+ EXPONENT
    ;

// Identifiers
IDENTIFIER
    : (LETTER | '_') (LETTER | DIGIT | '_' | '@' | ':')*
    ;

QUOTED_IDENTIFIER
    : '"' ( ~'"' | '""' )* '"'
    ;

BACKQUOTED_IDENTIFIER
    : '`' ( ~'`' | '``' )* '`'
    ;

// Fragments
fragment EXPONENT
    : [eE] [+-]? DIGIT+
    ;

fragment DIGIT
    : [0-9]
    ;

fragment LETTER
    : [a-zA-Z]
    ;

// Whitespace and comments
SIMPLE_COMMENT
    : '--' ~[\r\n]* '\r'? '\n'? -> channel(HIDDEN)
    ;

BRACKETED_COMMENT
    : '/*' .*? '*/' -> channel(HIDDEN)
    ;

WS
    : [ \r\n\t]+ -> channel(HIDDEN)
    ;

UNRECOGNIZED
    : .
    ;
