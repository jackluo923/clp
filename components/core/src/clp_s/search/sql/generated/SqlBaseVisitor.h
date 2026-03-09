
// Generated from Sql.g4 by ANTLR 4.13.2

#pragma once


#include "antlr4-runtime.h"
#include "SqlVisitor.h"


namespace clp_s::search::sql::generated {

/**
 * This class provides an empty implementation of SqlVisitor, which can be
 * extended to create a visitor which only needs to handle a subset of the available methods.
 */
class  SqlBaseVisitor : public SqlVisitor {
public:

  virtual std::any visitSingleStatement(SqlParser::SingleStatementContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitStatementDefault(SqlParser::StatementDefaultContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitQuery(SqlParser::QueryContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitQuerySpecification(SqlParser::QuerySpecificationContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitSetQuantifier(SqlParser::SetQuantifierContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitSelectSingle(SqlParser::SelectSingleContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitSelectAll(SqlParser::SelectAllContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitExpression(SqlParser::ExpressionContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitLogicalNot(SqlParser::LogicalNotContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitPredicated(SqlParser::PredicatedContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitLogicalBinary(SqlParser::LogicalBinaryContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitComparison(SqlParser::ComparisonContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitBetween(SqlParser::BetweenContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitInList(SqlParser::InListContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitLike(SqlParser::LikeContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitNullPredicate(SqlParser::NullPredicateContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitValueExpressionDefault(SqlParser::ValueExpressionDefaultContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitArithmeticBinary(SqlParser::ArithmeticBinaryContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitArithmeticUnary(SqlParser::ArithmeticUnaryContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitTimestampLiteral(SqlParser::TimestampLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitDereference(SqlParser::DereferenceContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggMax(SqlParser::AggMaxContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitColumnReference(SqlParser::ColumnReferenceContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitNullLiteral(SqlParser::NullLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggCountStar(SqlParser::AggCountStarContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitClpGetBool(SqlParser::ClpGetBoolContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitClpGetFloat(SqlParser::ClpGetFloatContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggSum(SqlParser::AggSumContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitClpGetString(SqlParser::ClpGetStringContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggMin(SqlParser::AggMinContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggAvg(SqlParser::AggAvgContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitParenthesizedExpression(SqlParser::ParenthesizedExpressionContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitStringLiteral(SqlParser::StringLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggCount(SqlParser::AggCountContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitDateLiteral(SqlParser::DateLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitClpGetJsonString(SqlParser::ClpGetJsonStringContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitClpGetInt(SqlParser::ClpGetIntContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitClpWildcardColumn(SqlParser::ClpWildcardColumnContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitAggArbitrary(SqlParser::AggArbitraryContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitNumericLiteral(SqlParser::NumericLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitBooleanLiteral(SqlParser::BooleanLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitQualifiedName(SqlParser::QualifiedNameContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitUnquotedIdentifier(SqlParser::UnquotedIdentifierContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitQuotedIdentifier(SqlParser::QuotedIdentifierContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitBackQuotedIdentifier(SqlParser::BackQuotedIdentifierContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitDecimalLiteral(SqlParser::DecimalLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitDoubleLiteral(SqlParser::DoubleLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitIntegerLiteral(SqlParser::IntegerLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitBasicStringLiteral(SqlParser::BasicStringLiteralContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitBooleanValue(SqlParser::BooleanValueContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitComparisonOperator(SqlParser::ComparisonOperatorContext *ctx) override {
    return visitChildren(ctx);
  }

  virtual std::any visitNonReserved(SqlParser::NonReservedContext *ctx) override {
    return visitChildren(ctx);
  }


};

}  // namespace clp_s::search::sql::generated
