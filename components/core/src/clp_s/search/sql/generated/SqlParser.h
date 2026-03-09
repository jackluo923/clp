
// Generated from Sql.g4 by ANTLR 4.13.2

#pragma once


#include "antlr4-runtime.h"


namespace clp_s::search::sql::generated {


class  SqlParser : public antlr4::Parser {
public:
  enum {
    T__0 = 1, T__1 = 2, T__2 = 3, T__3 = 4, CLP_GET_INT = 5, CLP_GET_FLOAT = 6, 
    CLP_GET_STRING = 7, CLP_GET_BOOL = 8, CLP_GET_JSON_STRING = 9, CLP_WILDCARD_COLUMN = 10, 
    ALL = 11, AND = 12, ANY_VALUE = 13, ARBITRARY = 14, AS = 15, AVG = 16, 
    BETWEEN = 17, COUNT = 18, DATE = 19, DISTINCT = 20, FALSE = 21, FROM = 22, 
    IN = 23, IS = 24, LIKE = 25, LIMIT = 26, MAX = 27, MIN = 28, NOT = 29, 
    NULL_ = 30, OR = 31, SELECT = 32, SUM = 33, TIMESTAMP = 34, TRUE = 35, 
    WHERE = 36, EQ = 37, NEQ = 38, LT = 39, LTE = 40, GT = 41, GTE = 42, 
    PLUS = 43, MINUS = 44, ASTERISK = 45, SLASH = 46, PERCENT = 47, STRING = 48, 
    INTEGER_VALUE = 49, DECIMAL_VALUE = 50, DOUBLE_VALUE = 51, IDENTIFIER = 52, 
    QUOTED_IDENTIFIER = 53, BACKQUOTED_IDENTIFIER = 54, SIMPLE_COMMENT = 55, 
    BRACKETED_COMMENT = 56, WS = 57, UNRECOGNIZED = 58
  };

  enum {
    RuleSingleStatement = 0, RuleStatement = 1, RuleQuery = 2, RuleQuerySpecification = 3, 
    RuleSetQuantifier = 4, RuleSelectItem = 5, RuleExpression = 6, RuleBooleanExpression = 7, 
    RulePredicate = 8, RuleValueExpression = 9, RulePrimaryExpression = 10, 
    RuleQualifiedName = 11, RuleIdentifier = 12, RuleNumber = 13, RuleString = 14, 
    RuleBooleanValue = 15, RuleComparisonOperator = 16, RuleNonReserved = 17
  };

  explicit SqlParser(antlr4::TokenStream *input);

  SqlParser(antlr4::TokenStream *input, const antlr4::atn::ParserATNSimulatorOptions &options);

  ~SqlParser() override;

  std::string getGrammarFileName() const override;

  const antlr4::atn::ATN& getATN() const override;

  const std::vector<std::string>& getRuleNames() const override;

  const antlr4::dfa::Vocabulary& getVocabulary() const override;

  antlr4::atn::SerializedATNView getSerializedATN() const override;


  class SingleStatementContext;
  class StatementContext;
  class QueryContext;
  class QuerySpecificationContext;
  class SetQuantifierContext;
  class SelectItemContext;
  class ExpressionContext;
  class BooleanExpressionContext;
  class PredicateContext;
  class ValueExpressionContext;
  class PrimaryExpressionContext;
  class QualifiedNameContext;
  class IdentifierContext;
  class NumberContext;
  class StringContext;
  class BooleanValueContext;
  class ComparisonOperatorContext;
  class NonReservedContext; 

  class  SingleStatementContext : public antlr4::ParserRuleContext {
  public:
    SingleStatementContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    StatementContext *statement();
    antlr4::tree::TerminalNode *EOF();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  SingleStatementContext* singleStatement();

  class  StatementContext : public antlr4::ParserRuleContext {
  public:
    StatementContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    StatementContext() = default;
    void copyFrom(StatementContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  StatementDefaultContext : public StatementContext {
  public:
    StatementDefaultContext(StatementContext *ctx);

    QueryContext *query();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  StatementContext* statement();

  class  QueryContext : public antlr4::ParserRuleContext {
  public:
    antlr4::Token *limit = nullptr;
    QueryContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    QuerySpecificationContext *querySpecification();
    antlr4::tree::TerminalNode *LIMIT();
    antlr4::tree::TerminalNode *INTEGER_VALUE();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  QueryContext* query();

  class  QuerySpecificationContext : public antlr4::ParserRuleContext {
  public:
    SqlParser::QualifiedNameContext *tableName = nullptr;
    SqlParser::BooleanExpressionContext *where = nullptr;
    QuerySpecificationContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    antlr4::tree::TerminalNode *SELECT();
    std::vector<SelectItemContext *> selectItem();
    SelectItemContext* selectItem(size_t i);
    SetQuantifierContext *setQuantifier();
    antlr4::tree::TerminalNode *FROM();
    antlr4::tree::TerminalNode *WHERE();
    QualifiedNameContext *qualifiedName();
    BooleanExpressionContext *booleanExpression();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  QuerySpecificationContext* querySpecification();

  class  SetQuantifierContext : public antlr4::ParserRuleContext {
  public:
    SetQuantifierContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    antlr4::tree::TerminalNode *DISTINCT();
    antlr4::tree::TerminalNode *ALL();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  SetQuantifierContext* setQuantifier();

  class  SelectItemContext : public antlr4::ParserRuleContext {
  public:
    SelectItemContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    SelectItemContext() = default;
    void copyFrom(SelectItemContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  SelectAllContext : public SelectItemContext {
  public:
    SelectAllContext(SelectItemContext *ctx);

    antlr4::tree::TerminalNode *ASTERISK();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  SelectSingleContext : public SelectItemContext {
  public:
    SelectSingleContext(SelectItemContext *ctx);

    ExpressionContext *expression();
    IdentifierContext *identifier();
    antlr4::tree::TerminalNode *AS();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  SelectItemContext* selectItem();

  class  ExpressionContext : public antlr4::ParserRuleContext {
  public:
    ExpressionContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    BooleanExpressionContext *booleanExpression();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  ExpressionContext* expression();

  class  BooleanExpressionContext : public antlr4::ParserRuleContext {
  public:
    BooleanExpressionContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    BooleanExpressionContext() = default;
    void copyFrom(BooleanExpressionContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  LogicalNotContext : public BooleanExpressionContext {
  public:
    LogicalNotContext(BooleanExpressionContext *ctx);

    antlr4::tree::TerminalNode *NOT();
    BooleanExpressionContext *booleanExpression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  PredicatedContext : public BooleanExpressionContext {
  public:
    PredicatedContext(BooleanExpressionContext *ctx);

    SqlParser::ValueExpressionContext *valueExpressionContext = nullptr;
    ValueExpressionContext *valueExpression();
    PredicateContext *predicate();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  LogicalBinaryContext : public BooleanExpressionContext {
  public:
    LogicalBinaryContext(BooleanExpressionContext *ctx);

    SqlParser::BooleanExpressionContext *left = nullptr;
    antlr4::Token *operator_ = nullptr;
    SqlParser::BooleanExpressionContext *right = nullptr;
    std::vector<BooleanExpressionContext *> booleanExpression();
    BooleanExpressionContext* booleanExpression(size_t i);
    antlr4::tree::TerminalNode *AND();
    antlr4::tree::TerminalNode *OR();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  BooleanExpressionContext* booleanExpression();
  BooleanExpressionContext* booleanExpression(int precedence);
  class  PredicateContext : public antlr4::ParserRuleContext {
  public:
    antlr4::ParserRuleContext* value;
    PredicateContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    PredicateContext(antlr4::ParserRuleContext *parent, size_t invokingState, antlr4::ParserRuleContext* value);
   
    PredicateContext() = default;
    void copyFrom(PredicateContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  ComparisonContext : public PredicateContext {
  public:
    ComparisonContext(PredicateContext *ctx);

    SqlParser::ValueExpressionContext *right = nullptr;
    ComparisonOperatorContext *comparisonOperator();
    ValueExpressionContext *valueExpression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  LikeContext : public PredicateContext {
  public:
    LikeContext(PredicateContext *ctx);

    SqlParser::ValueExpressionContext *pattern = nullptr;
    antlr4::tree::TerminalNode *LIKE();
    ValueExpressionContext *valueExpression();
    antlr4::tree::TerminalNode *NOT();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  InListContext : public PredicateContext {
  public:
    InListContext(PredicateContext *ctx);

    antlr4::tree::TerminalNode *IN();
    std::vector<ExpressionContext *> expression();
    ExpressionContext* expression(size_t i);
    antlr4::tree::TerminalNode *NOT();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  NullPredicateContext : public PredicateContext {
  public:
    NullPredicateContext(PredicateContext *ctx);

    antlr4::tree::TerminalNode *IS();
    antlr4::tree::TerminalNode *NULL_();
    antlr4::tree::TerminalNode *NOT();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  BetweenContext : public PredicateContext {
  public:
    BetweenContext(PredicateContext *ctx);

    SqlParser::ValueExpressionContext *lower = nullptr;
    SqlParser::ValueExpressionContext *upper = nullptr;
    antlr4::tree::TerminalNode *BETWEEN();
    antlr4::tree::TerminalNode *AND();
    std::vector<ValueExpressionContext *> valueExpression();
    ValueExpressionContext* valueExpression(size_t i);
    antlr4::tree::TerminalNode *NOT();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  PredicateContext* predicate(antlr4::ParserRuleContext* value);

  class  ValueExpressionContext : public antlr4::ParserRuleContext {
  public:
    ValueExpressionContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    ValueExpressionContext() = default;
    void copyFrom(ValueExpressionContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  ValueExpressionDefaultContext : public ValueExpressionContext {
  public:
    ValueExpressionDefaultContext(ValueExpressionContext *ctx);

    PrimaryExpressionContext *primaryExpression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ArithmeticBinaryContext : public ValueExpressionContext {
  public:
    ArithmeticBinaryContext(ValueExpressionContext *ctx);

    SqlParser::ValueExpressionContext *left = nullptr;
    antlr4::Token *operator_ = nullptr;
    SqlParser::ValueExpressionContext *right = nullptr;
    std::vector<ValueExpressionContext *> valueExpression();
    ValueExpressionContext* valueExpression(size_t i);
    antlr4::tree::TerminalNode *ASTERISK();
    antlr4::tree::TerminalNode *SLASH();
    antlr4::tree::TerminalNode *PERCENT();
    antlr4::tree::TerminalNode *PLUS();
    antlr4::tree::TerminalNode *MINUS();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ArithmeticUnaryContext : public ValueExpressionContext {
  public:
    ArithmeticUnaryContext(ValueExpressionContext *ctx);

    antlr4::Token *operator_ = nullptr;
    ValueExpressionContext *valueExpression();
    antlr4::tree::TerminalNode *MINUS();
    antlr4::tree::TerminalNode *PLUS();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  ValueExpressionContext* valueExpression();
  ValueExpressionContext* valueExpression(int precedence);
  class  PrimaryExpressionContext : public antlr4::ParserRuleContext {
  public:
    PrimaryExpressionContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    PrimaryExpressionContext() = default;
    void copyFrom(PrimaryExpressionContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  TimestampLiteralContext : public PrimaryExpressionContext {
  public:
    TimestampLiteralContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *TIMESTAMP();
    StringContext *string();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  DereferenceContext : public PrimaryExpressionContext {
  public:
    DereferenceContext(PrimaryExpressionContext *ctx);

    SqlParser::PrimaryExpressionContext *base = nullptr;
    SqlParser::IdentifierContext *fieldName = nullptr;
    PrimaryExpressionContext *primaryExpression();
    IdentifierContext *identifier();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggMaxContext : public PrimaryExpressionContext {
  public:
    AggMaxContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *MAX();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ColumnReferenceContext : public PrimaryExpressionContext {
  public:
    ColumnReferenceContext(PrimaryExpressionContext *ctx);

    IdentifierContext *identifier();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  NullLiteralContext : public PrimaryExpressionContext {
  public:
    NullLiteralContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *NULL_();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggCountStarContext : public PrimaryExpressionContext {
  public:
    AggCountStarContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *COUNT();
    antlr4::tree::TerminalNode *ASTERISK();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ClpGetBoolContext : public PrimaryExpressionContext {
  public:
    ClpGetBoolContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *CLP_GET_BOOL();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ClpGetFloatContext : public PrimaryExpressionContext {
  public:
    ClpGetFloatContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *CLP_GET_FLOAT();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggSumContext : public PrimaryExpressionContext {
  public:
    AggSumContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *SUM();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ClpGetStringContext : public PrimaryExpressionContext {
  public:
    ClpGetStringContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *CLP_GET_STRING();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggMinContext : public PrimaryExpressionContext {
  public:
    AggMinContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *MIN();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggAvgContext : public PrimaryExpressionContext {
  public:
    AggAvgContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *AVG();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ParenthesizedExpressionContext : public PrimaryExpressionContext {
  public:
    ParenthesizedExpressionContext(PrimaryExpressionContext *ctx);

    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  StringLiteralContext : public PrimaryExpressionContext {
  public:
    StringLiteralContext(PrimaryExpressionContext *ctx);

    StringContext *string();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggCountContext : public PrimaryExpressionContext {
  public:
    AggCountContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *COUNT();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  DateLiteralContext : public PrimaryExpressionContext {
  public:
    DateLiteralContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *DATE();
    StringContext *string();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ClpGetJsonStringContext : public PrimaryExpressionContext {
  public:
    ClpGetJsonStringContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *CLP_GET_JSON_STRING();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ClpGetIntContext : public PrimaryExpressionContext {
  public:
    ClpGetIntContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *CLP_GET_INT();
    ExpressionContext *expression();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  ClpWildcardColumnContext : public PrimaryExpressionContext {
  public:
    ClpWildcardColumnContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *CLP_WILDCARD_COLUMN();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  AggArbitraryContext : public PrimaryExpressionContext {
  public:
    AggArbitraryContext(PrimaryExpressionContext *ctx);

    antlr4::tree::TerminalNode *ARBITRARY();
    ExpressionContext *expression();
    antlr4::tree::TerminalNode *ANY_VALUE();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  NumericLiteralContext : public PrimaryExpressionContext {
  public:
    NumericLiteralContext(PrimaryExpressionContext *ctx);

    NumberContext *number();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  BooleanLiteralContext : public PrimaryExpressionContext {
  public:
    BooleanLiteralContext(PrimaryExpressionContext *ctx);

    BooleanValueContext *booleanValue();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  PrimaryExpressionContext* primaryExpression();
  PrimaryExpressionContext* primaryExpression(int precedence);
  class  QualifiedNameContext : public antlr4::ParserRuleContext {
  public:
    QualifiedNameContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    std::vector<IdentifierContext *> identifier();
    IdentifierContext* identifier(size_t i);


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  QualifiedNameContext* qualifiedName();

  class  IdentifierContext : public antlr4::ParserRuleContext {
  public:
    IdentifierContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    IdentifierContext() = default;
    void copyFrom(IdentifierContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  BackQuotedIdentifierContext : public IdentifierContext {
  public:
    BackQuotedIdentifierContext(IdentifierContext *ctx);

    antlr4::tree::TerminalNode *BACKQUOTED_IDENTIFIER();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  QuotedIdentifierContext : public IdentifierContext {
  public:
    QuotedIdentifierContext(IdentifierContext *ctx);

    antlr4::tree::TerminalNode *QUOTED_IDENTIFIER();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  UnquotedIdentifierContext : public IdentifierContext {
  public:
    UnquotedIdentifierContext(IdentifierContext *ctx);

    antlr4::tree::TerminalNode *IDENTIFIER();
    NonReservedContext *nonReserved();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  IdentifierContext* identifier();

  class  NumberContext : public antlr4::ParserRuleContext {
  public:
    NumberContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    NumberContext() = default;
    void copyFrom(NumberContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  DecimalLiteralContext : public NumberContext {
  public:
    DecimalLiteralContext(NumberContext *ctx);

    antlr4::tree::TerminalNode *DECIMAL_VALUE();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  DoubleLiteralContext : public NumberContext {
  public:
    DoubleLiteralContext(NumberContext *ctx);

    antlr4::tree::TerminalNode *DOUBLE_VALUE();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  class  IntegerLiteralContext : public NumberContext {
  public:
    IntegerLiteralContext(NumberContext *ctx);

    antlr4::tree::TerminalNode *INTEGER_VALUE();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  NumberContext* number();

  class  StringContext : public antlr4::ParserRuleContext {
  public:
    StringContext(antlr4::ParserRuleContext *parent, size_t invokingState);
   
    StringContext() = default;
    void copyFrom(StringContext *context);
    using antlr4::ParserRuleContext::copyFrom;

    virtual size_t getRuleIndex() const override;

   
  };

  class  BasicStringLiteralContext : public StringContext {
  public:
    BasicStringLiteralContext(StringContext *ctx);

    antlr4::tree::TerminalNode *STRING();

    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
  };

  StringContext* string();

  class  BooleanValueContext : public antlr4::ParserRuleContext {
  public:
    BooleanValueContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    antlr4::tree::TerminalNode *TRUE();
    antlr4::tree::TerminalNode *FALSE();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  BooleanValueContext* booleanValue();

  class  ComparisonOperatorContext : public antlr4::ParserRuleContext {
  public:
    ComparisonOperatorContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    antlr4::tree::TerminalNode *EQ();
    antlr4::tree::TerminalNode *NEQ();
    antlr4::tree::TerminalNode *LT();
    antlr4::tree::TerminalNode *LTE();
    antlr4::tree::TerminalNode *GT();
    antlr4::tree::TerminalNode *GTE();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  ComparisonOperatorContext* comparisonOperator();

  class  NonReservedContext : public antlr4::ParserRuleContext {
  public:
    NonReservedContext(antlr4::ParserRuleContext *parent, size_t invokingState);
    virtual size_t getRuleIndex() const override;
    antlr4::tree::TerminalNode *ALL();
    antlr4::tree::TerminalNode *ANY_VALUE();
    antlr4::tree::TerminalNode *ARBITRARY();
    antlr4::tree::TerminalNode *AS();
    antlr4::tree::TerminalNode *AVG();
    antlr4::tree::TerminalNode *COUNT();
    antlr4::tree::TerminalNode *DATE();
    antlr4::tree::TerminalNode *DISTINCT();
    antlr4::tree::TerminalNode *LIMIT();
    antlr4::tree::TerminalNode *MAX();
    antlr4::tree::TerminalNode *MIN();
    antlr4::tree::TerminalNode *SUM();
    antlr4::tree::TerminalNode *TIMESTAMP();


    virtual std::any accept(antlr4::tree::ParseTreeVisitor *visitor) override;
   
  };

  NonReservedContext* nonReserved();


  bool sempred(antlr4::RuleContext *_localctx, size_t ruleIndex, size_t predicateIndex) override;

  bool booleanExpressionSempred(BooleanExpressionContext *_localctx, size_t predicateIndex);
  bool valueExpressionSempred(ValueExpressionContext *_localctx, size_t predicateIndex);
  bool primaryExpressionSempred(PrimaryExpressionContext *_localctx, size_t predicateIndex);

  // By default the static state used to implement the parser is lazily initialized during the first
  // call to the constructor. You can call this function if you wish to initialize the static state
  // ahead of time.
  static void initialize();

private:
};

}  // namespace clp_s::search::sql::generated
