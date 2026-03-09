
// Generated from Sql.g4 by ANTLR 4.13.2

#pragma once


#include "antlr4-runtime.h"
#include "SqlParser.h"


namespace clp_s::search::sql::generated {

/**
 * This class defines an abstract visitor for a parse tree
 * produced by SqlParser.
 */
class  SqlVisitor : public antlr4::tree::AbstractParseTreeVisitor {
public:

  /**
   * Visit parse trees produced by SqlParser.
   */
    virtual std::any visitSingleStatement(SqlParser::SingleStatementContext *context) = 0;

    virtual std::any visitStatementDefault(SqlParser::StatementDefaultContext *context) = 0;

    virtual std::any visitQuery(SqlParser::QueryContext *context) = 0;

    virtual std::any visitQuerySpecification(SqlParser::QuerySpecificationContext *context) = 0;

    virtual std::any visitSetQuantifier(SqlParser::SetQuantifierContext *context) = 0;

    virtual std::any visitSelectSingle(SqlParser::SelectSingleContext *context) = 0;

    virtual std::any visitSelectAll(SqlParser::SelectAllContext *context) = 0;

    virtual std::any visitExpression(SqlParser::ExpressionContext *context) = 0;

    virtual std::any visitLogicalNot(SqlParser::LogicalNotContext *context) = 0;

    virtual std::any visitPredicated(SqlParser::PredicatedContext *context) = 0;

    virtual std::any visitLogicalBinary(SqlParser::LogicalBinaryContext *context) = 0;

    virtual std::any visitComparison(SqlParser::ComparisonContext *context) = 0;

    virtual std::any visitBetween(SqlParser::BetweenContext *context) = 0;

    virtual std::any visitInList(SqlParser::InListContext *context) = 0;

    virtual std::any visitLike(SqlParser::LikeContext *context) = 0;

    virtual std::any visitNullPredicate(SqlParser::NullPredicateContext *context) = 0;

    virtual std::any visitValueExpressionDefault(SqlParser::ValueExpressionDefaultContext *context) = 0;

    virtual std::any visitArithmeticBinary(SqlParser::ArithmeticBinaryContext *context) = 0;

    virtual std::any visitArithmeticUnary(SqlParser::ArithmeticUnaryContext *context) = 0;

    virtual std::any visitTimestampLiteral(SqlParser::TimestampLiteralContext *context) = 0;

    virtual std::any visitDereference(SqlParser::DereferenceContext *context) = 0;

    virtual std::any visitAggMax(SqlParser::AggMaxContext *context) = 0;

    virtual std::any visitColumnReference(SqlParser::ColumnReferenceContext *context) = 0;

    virtual std::any visitNullLiteral(SqlParser::NullLiteralContext *context) = 0;

    virtual std::any visitAggCountStar(SqlParser::AggCountStarContext *context) = 0;

    virtual std::any visitClpGetBool(SqlParser::ClpGetBoolContext *context) = 0;

    virtual std::any visitClpGetFloat(SqlParser::ClpGetFloatContext *context) = 0;

    virtual std::any visitAggSum(SqlParser::AggSumContext *context) = 0;

    virtual std::any visitClpGetString(SqlParser::ClpGetStringContext *context) = 0;

    virtual std::any visitAggMin(SqlParser::AggMinContext *context) = 0;

    virtual std::any visitAggAvg(SqlParser::AggAvgContext *context) = 0;

    virtual std::any visitParenthesizedExpression(SqlParser::ParenthesizedExpressionContext *context) = 0;

    virtual std::any visitStringLiteral(SqlParser::StringLiteralContext *context) = 0;

    virtual std::any visitAggCount(SqlParser::AggCountContext *context) = 0;

    virtual std::any visitDateLiteral(SqlParser::DateLiteralContext *context) = 0;

    virtual std::any visitClpGetJsonString(SqlParser::ClpGetJsonStringContext *context) = 0;

    virtual std::any visitClpGetInt(SqlParser::ClpGetIntContext *context) = 0;

    virtual std::any visitClpWildcardColumn(SqlParser::ClpWildcardColumnContext *context) = 0;

    virtual std::any visitAggArbitrary(SqlParser::AggArbitraryContext *context) = 0;

    virtual std::any visitNumericLiteral(SqlParser::NumericLiteralContext *context) = 0;

    virtual std::any visitBooleanLiteral(SqlParser::BooleanLiteralContext *context) = 0;

    virtual std::any visitQualifiedName(SqlParser::QualifiedNameContext *context) = 0;

    virtual std::any visitUnquotedIdentifier(SqlParser::UnquotedIdentifierContext *context) = 0;

    virtual std::any visitQuotedIdentifier(SqlParser::QuotedIdentifierContext *context) = 0;

    virtual std::any visitBackQuotedIdentifier(SqlParser::BackQuotedIdentifierContext *context) = 0;

    virtual std::any visitDecimalLiteral(SqlParser::DecimalLiteralContext *context) = 0;

    virtual std::any visitDoubleLiteral(SqlParser::DoubleLiteralContext *context) = 0;

    virtual std::any visitIntegerLiteral(SqlParser::IntegerLiteralContext *context) = 0;

    virtual std::any visitBasicStringLiteral(SqlParser::BasicStringLiteralContext *context) = 0;

    virtual std::any visitBooleanValue(SqlParser::BooleanValueContext *context) = 0;

    virtual std::any visitComparisonOperator(SqlParser::ComparisonOperatorContext *context) = 0;

    virtual std::any visitNonReserved(SqlParser::NonReservedContext *context) = 0;


};

}  // namespace clp_s::search::sql::generated
