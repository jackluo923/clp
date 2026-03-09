
// Generated from Sql.g4 by ANTLR 4.13.2


#include "SqlVisitor.h"

#include "SqlParser.h"


using namespace antlrcpp;
using namespace clp_s::search::sql::generated;

using namespace antlr4;

namespace {

struct SqlParserStaticData final {
  SqlParserStaticData(std::vector<std::string> ruleNames,
                        std::vector<std::string> literalNames,
                        std::vector<std::string> symbolicNames)
      : ruleNames(std::move(ruleNames)), literalNames(std::move(literalNames)),
        symbolicNames(std::move(symbolicNames)),
        vocabulary(this->literalNames, this->symbolicNames) {}

  SqlParserStaticData(const SqlParserStaticData&) = delete;
  SqlParserStaticData(SqlParserStaticData&&) = delete;
  SqlParserStaticData& operator=(const SqlParserStaticData&) = delete;
  SqlParserStaticData& operator=(SqlParserStaticData&&) = delete;

  std::vector<antlr4::dfa::DFA> decisionToDFA;
  antlr4::atn::PredictionContextCache sharedContextCache;
  const std::vector<std::string> ruleNames;
  const std::vector<std::string> literalNames;
  const std::vector<std::string> symbolicNames;
  const antlr4::dfa::Vocabulary vocabulary;
  antlr4::atn::SerializedATNView serializedATN;
  std::unique_ptr<antlr4::atn::ATN> atn;
};

::antlr4::internal::OnceFlag sqlParserOnceFlag;
#if ANTLR4_USE_THREAD_LOCAL_CACHE
static thread_local
#endif
std::unique_ptr<SqlParserStaticData> sqlParserStaticData = nullptr;

void sqlParserInitialize() {
#if ANTLR4_USE_THREAD_LOCAL_CACHE
  if (sqlParserStaticData != nullptr) {
    return;
  }
#else
  assert(sqlParserStaticData == nullptr);
#endif
  auto staticData = std::make_unique<SqlParserStaticData>(
    std::vector<std::string>{
      "singleStatement", "statement", "query", "querySpecification", "setQuantifier", 
      "selectItem", "expression", "booleanExpression", "predicate", "valueExpression", 
      "primaryExpression", "qualifiedName", "identifier", "number", "string", 
      "booleanValue", "comparisonOperator", "nonReserved"
    },
    std::vector<std::string>{
      "", "','", "'('", "')'", "'.'", "'CLP_GET_INT'", "'CLP_GET_FLOAT'", 
      "'CLP_GET_STRING'", "'CLP_GET_BOOL'", "'CLP_GET_JSON_STRING'", "'CLP_WILDCARD_COLUMN'", 
      "'ALL'", "'AND'", "'ANY_VALUE'", "'ARBITRARY'", "'AS'", "'AVG'", "'BETWEEN'", 
      "'COUNT'", "'DATE'", "'DISTINCT'", "'FALSE'", "'FROM'", "'IN'", "'IS'", 
      "'LIKE'", "'LIMIT'", "'MAX'", "'MIN'", "'NOT'", "'NULL'", "'OR'", 
      "'SELECT'", "'SUM'", "'TIMESTAMP'", "'TRUE'", "'WHERE'", "'='", "", 
      "'<'", "'<='", "'>'", "'>='", "'+'", "'-'", "'*'", "'/'", "'%'"
    },
    std::vector<std::string>{
      "", "", "", "", "", "CLP_GET_INT", "CLP_GET_FLOAT", "CLP_GET_STRING", 
      "CLP_GET_BOOL", "CLP_GET_JSON_STRING", "CLP_WILDCARD_COLUMN", "ALL", 
      "AND", "ANY_VALUE", "ARBITRARY", "AS", "AVG", "BETWEEN", "COUNT", 
      "DATE", "DISTINCT", "FALSE", "FROM", "IN", "IS", "LIKE", "LIMIT", 
      "MAX", "MIN", "NOT", "NULL", "OR", "SELECT", "SUM", "TIMESTAMP", "TRUE", 
      "WHERE", "EQ", "NEQ", "LT", "LTE", "GT", "GTE", "PLUS", "MINUS", "ASTERISK", 
      "SLASH", "PERCENT", "STRING", "INTEGER_VALUE", "DECIMAL_VALUE", "DOUBLE_VALUE", 
      "IDENTIFIER", "QUOTED_IDENTIFIER", "BACKQUOTED_IDENTIFIER", "SIMPLE_COMMENT", 
      "BRACKETED_COMMENT", "WS", "UNRECOGNIZED"
    }
  );
  static const int32_t serializedATNSegment[] = {
  	4,1,58,275,2,0,7,0,2,1,7,1,2,2,7,2,2,3,7,3,2,4,7,4,2,5,7,5,2,6,7,6,2,
  	7,7,7,2,8,7,8,2,9,7,9,2,10,7,10,2,11,7,11,2,12,7,12,2,13,7,13,2,14,7,
  	14,2,15,7,15,2,16,7,16,2,17,7,17,1,0,1,0,1,0,1,1,1,1,1,2,1,2,1,2,3,2,
  	45,8,2,1,3,1,3,3,3,49,8,3,1,3,1,3,1,3,5,3,54,8,3,10,3,12,3,57,9,3,1,3,
  	1,3,3,3,61,8,3,1,3,1,3,3,3,65,8,3,1,4,1,4,1,5,1,5,3,5,71,8,5,1,5,3,5,
  	74,8,5,1,5,3,5,77,8,5,1,6,1,6,1,7,1,7,1,7,3,7,84,8,7,1,7,1,7,3,7,88,8,
  	7,1,7,1,7,1,7,1,7,1,7,1,7,5,7,96,8,7,10,7,12,7,99,9,7,1,8,1,8,1,8,1,8,
  	3,8,105,8,8,1,8,1,8,1,8,1,8,1,8,1,8,3,8,113,8,8,1,8,1,8,1,8,1,8,1,8,5,
  	8,120,8,8,10,8,12,8,123,9,8,1,8,1,8,1,8,3,8,128,8,8,1,8,1,8,1,8,1,8,3,
  	8,134,8,8,1,8,3,8,137,8,8,1,9,1,9,1,9,1,9,3,9,143,8,9,1,9,1,9,1,9,1,9,
  	1,9,1,9,5,9,151,8,9,10,9,12,9,154,9,9,1,10,1,10,1,10,1,10,1,10,1,10,1,
  	10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,
  	10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,3,10,188,8,
  	10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,
  	10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,
  	10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,10,1,
  	10,1,10,1,10,1,10,1,10,1,10,1,10,3,10,238,8,10,1,10,1,10,1,10,5,10,243,
  	8,10,10,10,12,10,246,9,10,1,11,1,11,1,11,5,11,251,8,11,10,11,12,11,254,
  	9,11,1,12,1,12,1,12,1,12,3,12,260,8,12,1,13,1,13,1,13,3,13,265,8,13,1,
  	14,1,14,1,15,1,15,1,16,1,16,1,17,1,17,1,17,0,3,14,18,20,18,0,2,4,6,8,
  	10,12,14,16,18,20,22,24,26,28,30,32,34,0,6,2,0,11,11,20,20,1,0,43,44,
  	1,0,45,47,2,0,21,21,35,35,1,0,37,42,5,0,11,11,13,16,18,20,26,28,33,34,
  	309,0,36,1,0,0,0,2,39,1,0,0,0,4,41,1,0,0,0,6,46,1,0,0,0,8,66,1,0,0,0,
  	10,76,1,0,0,0,12,78,1,0,0,0,14,87,1,0,0,0,16,136,1,0,0,0,18,142,1,0,0,
  	0,20,237,1,0,0,0,22,247,1,0,0,0,24,259,1,0,0,0,26,264,1,0,0,0,28,266,
  	1,0,0,0,30,268,1,0,0,0,32,270,1,0,0,0,34,272,1,0,0,0,36,37,3,2,1,0,37,
  	38,5,0,0,1,38,1,1,0,0,0,39,40,3,4,2,0,40,3,1,0,0,0,41,44,3,6,3,0,42,43,
  	5,26,0,0,43,45,5,49,0,0,44,42,1,0,0,0,44,45,1,0,0,0,45,5,1,0,0,0,46,48,
  	5,32,0,0,47,49,3,8,4,0,48,47,1,0,0,0,48,49,1,0,0,0,49,50,1,0,0,0,50,55,
  	3,10,5,0,51,52,5,1,0,0,52,54,3,10,5,0,53,51,1,0,0,0,54,57,1,0,0,0,55,
  	53,1,0,0,0,55,56,1,0,0,0,56,60,1,0,0,0,57,55,1,0,0,0,58,59,5,22,0,0,59,
  	61,3,22,11,0,60,58,1,0,0,0,60,61,1,0,0,0,61,64,1,0,0,0,62,63,5,36,0,0,
  	63,65,3,14,7,0,64,62,1,0,0,0,64,65,1,0,0,0,65,7,1,0,0,0,66,67,7,0,0,0,
  	67,9,1,0,0,0,68,73,3,12,6,0,69,71,5,15,0,0,70,69,1,0,0,0,70,71,1,0,0,
  	0,71,72,1,0,0,0,72,74,3,24,12,0,73,70,1,0,0,0,73,74,1,0,0,0,74,77,1,0,
  	0,0,75,77,5,45,0,0,76,68,1,0,0,0,76,75,1,0,0,0,77,11,1,0,0,0,78,79,3,
  	14,7,0,79,13,1,0,0,0,80,81,6,7,-1,0,81,83,3,18,9,0,82,84,3,16,8,0,83,
  	82,1,0,0,0,83,84,1,0,0,0,84,88,1,0,0,0,85,86,5,29,0,0,86,88,3,14,7,3,
  	87,80,1,0,0,0,87,85,1,0,0,0,88,97,1,0,0,0,89,90,10,2,0,0,90,91,5,12,0,
  	0,91,96,3,14,7,3,92,93,10,1,0,0,93,94,5,31,0,0,94,96,3,14,7,2,95,89,1,
  	0,0,0,95,92,1,0,0,0,96,99,1,0,0,0,97,95,1,0,0,0,97,98,1,0,0,0,98,15,1,
  	0,0,0,99,97,1,0,0,0,100,101,3,32,16,0,101,102,3,18,9,0,102,137,1,0,0,
  	0,103,105,5,29,0,0,104,103,1,0,0,0,104,105,1,0,0,0,105,106,1,0,0,0,106,
  	107,5,17,0,0,107,108,3,18,9,0,108,109,5,12,0,0,109,110,3,18,9,0,110,137,
  	1,0,0,0,111,113,5,29,0,0,112,111,1,0,0,0,112,113,1,0,0,0,113,114,1,0,
  	0,0,114,115,5,23,0,0,115,116,5,2,0,0,116,121,3,12,6,0,117,118,5,1,0,0,
  	118,120,3,12,6,0,119,117,1,0,0,0,120,123,1,0,0,0,121,119,1,0,0,0,121,
  	122,1,0,0,0,122,124,1,0,0,0,123,121,1,0,0,0,124,125,5,3,0,0,125,137,1,
  	0,0,0,126,128,5,29,0,0,127,126,1,0,0,0,127,128,1,0,0,0,128,129,1,0,0,
  	0,129,130,5,25,0,0,130,137,3,18,9,0,131,133,5,24,0,0,132,134,5,29,0,0,
  	133,132,1,0,0,0,133,134,1,0,0,0,134,135,1,0,0,0,135,137,5,30,0,0,136,
  	100,1,0,0,0,136,104,1,0,0,0,136,112,1,0,0,0,136,127,1,0,0,0,136,131,1,
  	0,0,0,137,17,1,0,0,0,138,139,6,9,-1,0,139,143,3,20,10,0,140,141,7,1,0,
  	0,141,143,3,18,9,3,142,138,1,0,0,0,142,140,1,0,0,0,143,152,1,0,0,0,144,
  	145,10,2,0,0,145,146,7,2,0,0,146,151,3,18,9,3,147,148,10,1,0,0,148,149,
  	7,1,0,0,149,151,3,18,9,2,150,144,1,0,0,0,150,147,1,0,0,0,151,154,1,0,
  	0,0,152,150,1,0,0,0,152,153,1,0,0,0,153,19,1,0,0,0,154,152,1,0,0,0,155,
  	156,6,10,-1,0,156,238,5,30,0,0,157,238,3,26,13,0,158,238,3,30,15,0,159,
  	238,3,28,14,0,160,161,5,34,0,0,161,238,3,28,14,0,162,163,5,19,0,0,163,
  	238,3,28,14,0,164,165,5,5,0,0,165,166,5,2,0,0,166,167,3,12,6,0,167,168,
  	5,3,0,0,168,238,1,0,0,0,169,170,5,6,0,0,170,171,5,2,0,0,171,172,3,12,
  	6,0,172,173,5,3,0,0,173,238,1,0,0,0,174,175,5,7,0,0,175,176,5,2,0,0,176,
  	177,3,12,6,0,177,178,5,3,0,0,178,238,1,0,0,0,179,180,5,8,0,0,180,181,
  	5,2,0,0,181,182,3,12,6,0,182,183,5,3,0,0,183,238,1,0,0,0,184,185,5,9,
  	0,0,185,187,5,2,0,0,186,188,3,12,6,0,187,186,1,0,0,0,187,188,1,0,0,0,
  	188,189,1,0,0,0,189,238,5,3,0,0,190,191,5,10,0,0,191,192,5,2,0,0,192,
  	238,5,3,0,0,193,194,5,18,0,0,194,195,5,2,0,0,195,196,5,45,0,0,196,238,
  	5,3,0,0,197,198,5,18,0,0,198,199,5,2,0,0,199,200,3,12,6,0,200,201,5,3,
  	0,0,201,238,1,0,0,0,202,203,5,28,0,0,203,204,5,2,0,0,204,205,3,12,6,0,
  	205,206,5,3,0,0,206,238,1,0,0,0,207,208,5,27,0,0,208,209,5,2,0,0,209,
  	210,3,12,6,0,210,211,5,3,0,0,211,238,1,0,0,0,212,213,5,33,0,0,213,214,
  	5,2,0,0,214,215,3,12,6,0,215,216,5,3,0,0,216,238,1,0,0,0,217,218,5,16,
  	0,0,218,219,5,2,0,0,219,220,3,12,6,0,220,221,5,3,0,0,221,238,1,0,0,0,
  	222,223,5,14,0,0,223,224,5,2,0,0,224,225,3,12,6,0,225,226,5,3,0,0,226,
  	238,1,0,0,0,227,228,5,13,0,0,228,229,5,2,0,0,229,230,3,12,6,0,230,231,
  	5,3,0,0,231,238,1,0,0,0,232,238,3,24,12,0,233,234,5,2,0,0,234,235,3,12,
  	6,0,235,236,5,3,0,0,236,238,1,0,0,0,237,155,1,0,0,0,237,157,1,0,0,0,237,
  	158,1,0,0,0,237,159,1,0,0,0,237,160,1,0,0,0,237,162,1,0,0,0,237,164,1,
  	0,0,0,237,169,1,0,0,0,237,174,1,0,0,0,237,179,1,0,0,0,237,184,1,0,0,0,
  	237,190,1,0,0,0,237,193,1,0,0,0,237,197,1,0,0,0,237,202,1,0,0,0,237,207,
  	1,0,0,0,237,212,1,0,0,0,237,217,1,0,0,0,237,222,1,0,0,0,237,227,1,0,0,
  	0,237,232,1,0,0,0,237,233,1,0,0,0,238,244,1,0,0,0,239,240,10,2,0,0,240,
  	241,5,4,0,0,241,243,3,24,12,0,242,239,1,0,0,0,243,246,1,0,0,0,244,242,
  	1,0,0,0,244,245,1,0,0,0,245,21,1,0,0,0,246,244,1,0,0,0,247,252,3,24,12,
  	0,248,249,5,4,0,0,249,251,3,24,12,0,250,248,1,0,0,0,251,254,1,0,0,0,252,
  	250,1,0,0,0,252,253,1,0,0,0,253,23,1,0,0,0,254,252,1,0,0,0,255,260,5,
  	52,0,0,256,260,5,53,0,0,257,260,5,54,0,0,258,260,3,34,17,0,259,255,1,
  	0,0,0,259,256,1,0,0,0,259,257,1,0,0,0,259,258,1,0,0,0,260,25,1,0,0,0,
  	261,265,5,50,0,0,262,265,5,51,0,0,263,265,5,49,0,0,264,261,1,0,0,0,264,
  	262,1,0,0,0,264,263,1,0,0,0,265,27,1,0,0,0,266,267,5,48,0,0,267,29,1,
  	0,0,0,268,269,7,3,0,0,269,31,1,0,0,0,270,271,7,4,0,0,271,33,1,0,0,0,272,
  	273,7,5,0,0,273,35,1,0,0,0,27,44,48,55,60,64,70,73,76,83,87,95,97,104,
  	112,121,127,133,136,142,150,152,187,237,244,252,259,264
  };
  staticData->serializedATN = antlr4::atn::SerializedATNView(serializedATNSegment, sizeof(serializedATNSegment) / sizeof(serializedATNSegment[0]));

  antlr4::atn::ATNDeserializer deserializer;
  staticData->atn = deserializer.deserialize(staticData->serializedATN);

  const size_t count = staticData->atn->getNumberOfDecisions();
  staticData->decisionToDFA.reserve(count);
  for (size_t i = 0; i < count; i++) { 
    staticData->decisionToDFA.emplace_back(staticData->atn->getDecisionState(i), i);
  }
  sqlParserStaticData = std::move(staticData);
}

}

SqlParser::SqlParser(TokenStream *input) : SqlParser(input, antlr4::atn::ParserATNSimulatorOptions()) {}

SqlParser::SqlParser(TokenStream *input, const antlr4::atn::ParserATNSimulatorOptions &options) : Parser(input) {
  SqlParser::initialize();
  _interpreter = new atn::ParserATNSimulator(this, *sqlParserStaticData->atn, sqlParserStaticData->decisionToDFA, sqlParserStaticData->sharedContextCache, options);
}

SqlParser::~SqlParser() {
  delete _interpreter;
}

const atn::ATN& SqlParser::getATN() const {
  return *sqlParserStaticData->atn;
}

std::string SqlParser::getGrammarFileName() const {
  return "Sql.g4";
}

const std::vector<std::string>& SqlParser::getRuleNames() const {
  return sqlParserStaticData->ruleNames;
}

const dfa::Vocabulary& SqlParser::getVocabulary() const {
  return sqlParserStaticData->vocabulary;
}

antlr4::atn::SerializedATNView SqlParser::getSerializedATN() const {
  return sqlParserStaticData->serializedATN;
}


//----------------- SingleStatementContext ------------------------------------------------------------------

SqlParser::SingleStatementContext::SingleStatementContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

SqlParser::StatementContext* SqlParser::SingleStatementContext::statement() {
  return getRuleContext<SqlParser::StatementContext>(0);
}

tree::TerminalNode* SqlParser::SingleStatementContext::EOF() {
  return getToken(SqlParser::EOF, 0);
}


size_t SqlParser::SingleStatementContext::getRuleIndex() const {
  return SqlParser::RuleSingleStatement;
}


std::any SqlParser::SingleStatementContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitSingleStatement(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::SingleStatementContext* SqlParser::singleStatement() {
  SingleStatementContext *_localctx = _tracker.createInstance<SingleStatementContext>(_ctx, getState());
  enterRule(_localctx, 0, SqlParser::RuleSingleStatement);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(36);
    statement();
    setState(37);
    match(SqlParser::EOF);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- StatementContext ------------------------------------------------------------------

SqlParser::StatementContext::StatementContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::StatementContext::getRuleIndex() const {
  return SqlParser::RuleStatement;
}

void SqlParser::StatementContext::copyFrom(StatementContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- StatementDefaultContext ------------------------------------------------------------------

SqlParser::QueryContext* SqlParser::StatementDefaultContext::query() {
  return getRuleContext<SqlParser::QueryContext>(0);
}

SqlParser::StatementDefaultContext::StatementDefaultContext(StatementContext *ctx) { copyFrom(ctx); }


std::any SqlParser::StatementDefaultContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitStatementDefault(this);
  else
    return visitor->visitChildren(this);
}
SqlParser::StatementContext* SqlParser::statement() {
  StatementContext *_localctx = _tracker.createInstance<StatementContext>(_ctx, getState());
  enterRule(_localctx, 2, SqlParser::RuleStatement);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    _localctx = _tracker.createInstance<SqlParser::StatementDefaultContext>(_localctx);
    enterOuterAlt(_localctx, 1);
    setState(39);
    query();
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- QueryContext ------------------------------------------------------------------

SqlParser::QueryContext::QueryContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

SqlParser::QuerySpecificationContext* SqlParser::QueryContext::querySpecification() {
  return getRuleContext<SqlParser::QuerySpecificationContext>(0);
}

tree::TerminalNode* SqlParser::QueryContext::LIMIT() {
  return getToken(SqlParser::LIMIT, 0);
}

tree::TerminalNode* SqlParser::QueryContext::INTEGER_VALUE() {
  return getToken(SqlParser::INTEGER_VALUE, 0);
}


size_t SqlParser::QueryContext::getRuleIndex() const {
  return SqlParser::RuleQuery;
}


std::any SqlParser::QueryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitQuery(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::QueryContext* SqlParser::query() {
  QueryContext *_localctx = _tracker.createInstance<QueryContext>(_ctx, getState());
  enterRule(_localctx, 4, SqlParser::RuleQuery);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(41);
    querySpecification();
    setState(44);
    _errHandler->sync(this);

    _la = _input->LA(1);
    if (_la == SqlParser::LIMIT) {
      setState(42);
      match(SqlParser::LIMIT);
      setState(43);
      antlrcpp::downCast<QueryContext *>(_localctx)->limit = match(SqlParser::INTEGER_VALUE);
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- QuerySpecificationContext ------------------------------------------------------------------

SqlParser::QuerySpecificationContext::QuerySpecificationContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* SqlParser::QuerySpecificationContext::SELECT() {
  return getToken(SqlParser::SELECT, 0);
}

std::vector<SqlParser::SelectItemContext *> SqlParser::QuerySpecificationContext::selectItem() {
  return getRuleContexts<SqlParser::SelectItemContext>();
}

SqlParser::SelectItemContext* SqlParser::QuerySpecificationContext::selectItem(size_t i) {
  return getRuleContext<SqlParser::SelectItemContext>(i);
}

SqlParser::SetQuantifierContext* SqlParser::QuerySpecificationContext::setQuantifier() {
  return getRuleContext<SqlParser::SetQuantifierContext>(0);
}

tree::TerminalNode* SqlParser::QuerySpecificationContext::FROM() {
  return getToken(SqlParser::FROM, 0);
}

tree::TerminalNode* SqlParser::QuerySpecificationContext::WHERE() {
  return getToken(SqlParser::WHERE, 0);
}

SqlParser::QualifiedNameContext* SqlParser::QuerySpecificationContext::qualifiedName() {
  return getRuleContext<SqlParser::QualifiedNameContext>(0);
}

SqlParser::BooleanExpressionContext* SqlParser::QuerySpecificationContext::booleanExpression() {
  return getRuleContext<SqlParser::BooleanExpressionContext>(0);
}


size_t SqlParser::QuerySpecificationContext::getRuleIndex() const {
  return SqlParser::RuleQuerySpecification;
}


std::any SqlParser::QuerySpecificationContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitQuerySpecification(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::QuerySpecificationContext* SqlParser::querySpecification() {
  QuerySpecificationContext *_localctx = _tracker.createInstance<QuerySpecificationContext>(_ctx, getState());
  enterRule(_localctx, 6, SqlParser::RuleQuerySpecification);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(46);
    match(SqlParser::SELECT);
    setState(48);
    _errHandler->sync(this);

    switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 1, _ctx)) {
    case 1: {
      setState(47);
      setQuantifier();
      break;
    }

    default:
      break;
    }
    setState(50);
    selectItem();
    setState(55);
    _errHandler->sync(this);
    _la = _input->LA(1);
    while (_la == SqlParser::T__0) {
      setState(51);
      match(SqlParser::T__0);
      setState(52);
      selectItem();
      setState(57);
      _errHandler->sync(this);
      _la = _input->LA(1);
    }
    setState(60);
    _errHandler->sync(this);

    _la = _input->LA(1);
    if (_la == SqlParser::FROM) {
      setState(58);
      match(SqlParser::FROM);
      setState(59);
      antlrcpp::downCast<QuerySpecificationContext *>(_localctx)->tableName = qualifiedName();
    }
    setState(64);
    _errHandler->sync(this);

    _la = _input->LA(1);
    if (_la == SqlParser::WHERE) {
      setState(62);
      match(SqlParser::WHERE);
      setState(63);
      antlrcpp::downCast<QuerySpecificationContext *>(_localctx)->where = booleanExpression(0);
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- SetQuantifierContext ------------------------------------------------------------------

SqlParser::SetQuantifierContext::SetQuantifierContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* SqlParser::SetQuantifierContext::DISTINCT() {
  return getToken(SqlParser::DISTINCT, 0);
}

tree::TerminalNode* SqlParser::SetQuantifierContext::ALL() {
  return getToken(SqlParser::ALL, 0);
}


size_t SqlParser::SetQuantifierContext::getRuleIndex() const {
  return SqlParser::RuleSetQuantifier;
}


std::any SqlParser::SetQuantifierContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitSetQuantifier(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::SetQuantifierContext* SqlParser::setQuantifier() {
  SetQuantifierContext *_localctx = _tracker.createInstance<SetQuantifierContext>(_ctx, getState());
  enterRule(_localctx, 8, SqlParser::RuleSetQuantifier);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(66);
    _la = _input->LA(1);
    if (!(_la == SqlParser::ALL

    || _la == SqlParser::DISTINCT)) {
    _errHandler->recoverInline(this);
    }
    else {
      _errHandler->reportMatch(this);
      consume();
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- SelectItemContext ------------------------------------------------------------------

SqlParser::SelectItemContext::SelectItemContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::SelectItemContext::getRuleIndex() const {
  return SqlParser::RuleSelectItem;
}

void SqlParser::SelectItemContext::copyFrom(SelectItemContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- SelectAllContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::SelectAllContext::ASTERISK() {
  return getToken(SqlParser::ASTERISK, 0);
}

SqlParser::SelectAllContext::SelectAllContext(SelectItemContext *ctx) { copyFrom(ctx); }


std::any SqlParser::SelectAllContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitSelectAll(this);
  else
    return visitor->visitChildren(this);
}
//----------------- SelectSingleContext ------------------------------------------------------------------

SqlParser::ExpressionContext* SqlParser::SelectSingleContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::IdentifierContext* SqlParser::SelectSingleContext::identifier() {
  return getRuleContext<SqlParser::IdentifierContext>(0);
}

tree::TerminalNode* SqlParser::SelectSingleContext::AS() {
  return getToken(SqlParser::AS, 0);
}

SqlParser::SelectSingleContext::SelectSingleContext(SelectItemContext *ctx) { copyFrom(ctx); }


std::any SqlParser::SelectSingleContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitSelectSingle(this);
  else
    return visitor->visitChildren(this);
}
SqlParser::SelectItemContext* SqlParser::selectItem() {
  SelectItemContext *_localctx = _tracker.createInstance<SelectItemContext>(_ctx, getState());
  enterRule(_localctx, 10, SqlParser::RuleSelectItem);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    setState(76);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case SqlParser::T__1:
      case SqlParser::CLP_GET_INT:
      case SqlParser::CLP_GET_FLOAT:
      case SqlParser::CLP_GET_STRING:
      case SqlParser::CLP_GET_BOOL:
      case SqlParser::CLP_GET_JSON_STRING:
      case SqlParser::CLP_WILDCARD_COLUMN:
      case SqlParser::ALL:
      case SqlParser::ANY_VALUE:
      case SqlParser::ARBITRARY:
      case SqlParser::AS:
      case SqlParser::AVG:
      case SqlParser::COUNT:
      case SqlParser::DATE:
      case SqlParser::DISTINCT:
      case SqlParser::FALSE:
      case SqlParser::LIMIT:
      case SqlParser::MAX:
      case SqlParser::MIN:
      case SqlParser::NOT:
      case SqlParser::NULL_:
      case SqlParser::SUM:
      case SqlParser::TIMESTAMP:
      case SqlParser::TRUE:
      case SqlParser::PLUS:
      case SqlParser::MINUS:
      case SqlParser::STRING:
      case SqlParser::INTEGER_VALUE:
      case SqlParser::DECIMAL_VALUE:
      case SqlParser::DOUBLE_VALUE:
      case SqlParser::IDENTIFIER:
      case SqlParser::QUOTED_IDENTIFIER:
      case SqlParser::BACKQUOTED_IDENTIFIER: {
        _localctx = _tracker.createInstance<SqlParser::SelectSingleContext>(_localctx);
        enterOuterAlt(_localctx, 1);
        setState(68);
        expression();
        setState(73);
        _errHandler->sync(this);

        switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 6, _ctx)) {
        case 1: {
          setState(70);
          _errHandler->sync(this);

          switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 5, _ctx)) {
          case 1: {
            setState(69);
            match(SqlParser::AS);
            break;
          }

          default:
            break;
          }
          setState(72);
          identifier();
          break;
        }

        default:
          break;
        }
        break;
      }

      case SqlParser::ASTERISK: {
        _localctx = _tracker.createInstance<SqlParser::SelectAllContext>(_localctx);
        enterOuterAlt(_localctx, 2);
        setState(75);
        match(SqlParser::ASTERISK);
        break;
      }

    default:
      throw NoViableAltException(this);
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- ExpressionContext ------------------------------------------------------------------

SqlParser::ExpressionContext::ExpressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

SqlParser::BooleanExpressionContext* SqlParser::ExpressionContext::booleanExpression() {
  return getRuleContext<SqlParser::BooleanExpressionContext>(0);
}


size_t SqlParser::ExpressionContext::getRuleIndex() const {
  return SqlParser::RuleExpression;
}


std::any SqlParser::ExpressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitExpression(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::ExpressionContext* SqlParser::expression() {
  ExpressionContext *_localctx = _tracker.createInstance<ExpressionContext>(_ctx, getState());
  enterRule(_localctx, 12, SqlParser::RuleExpression);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(78);
    booleanExpression(0);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- BooleanExpressionContext ------------------------------------------------------------------

SqlParser::BooleanExpressionContext::BooleanExpressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::BooleanExpressionContext::getRuleIndex() const {
  return SqlParser::RuleBooleanExpression;
}

void SqlParser::BooleanExpressionContext::copyFrom(BooleanExpressionContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- LogicalNotContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::LogicalNotContext::NOT() {
  return getToken(SqlParser::NOT, 0);
}

SqlParser::BooleanExpressionContext* SqlParser::LogicalNotContext::booleanExpression() {
  return getRuleContext<SqlParser::BooleanExpressionContext>(0);
}

SqlParser::LogicalNotContext::LogicalNotContext(BooleanExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::LogicalNotContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitLogicalNot(this);
  else
    return visitor->visitChildren(this);
}
//----------------- PredicatedContext ------------------------------------------------------------------

SqlParser::ValueExpressionContext* SqlParser::PredicatedContext::valueExpression() {
  return getRuleContext<SqlParser::ValueExpressionContext>(0);
}

SqlParser::PredicateContext* SqlParser::PredicatedContext::predicate() {
  return getRuleContext<SqlParser::PredicateContext>(0);
}

SqlParser::PredicatedContext::PredicatedContext(BooleanExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::PredicatedContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitPredicated(this);
  else
    return visitor->visitChildren(this);
}
//----------------- LogicalBinaryContext ------------------------------------------------------------------

std::vector<SqlParser::BooleanExpressionContext *> SqlParser::LogicalBinaryContext::booleanExpression() {
  return getRuleContexts<SqlParser::BooleanExpressionContext>();
}

SqlParser::BooleanExpressionContext* SqlParser::LogicalBinaryContext::booleanExpression(size_t i) {
  return getRuleContext<SqlParser::BooleanExpressionContext>(i);
}

tree::TerminalNode* SqlParser::LogicalBinaryContext::AND() {
  return getToken(SqlParser::AND, 0);
}

tree::TerminalNode* SqlParser::LogicalBinaryContext::OR() {
  return getToken(SqlParser::OR, 0);
}

SqlParser::LogicalBinaryContext::LogicalBinaryContext(BooleanExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::LogicalBinaryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitLogicalBinary(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::BooleanExpressionContext* SqlParser::booleanExpression() {
   return booleanExpression(0);
}

SqlParser::BooleanExpressionContext* SqlParser::booleanExpression(int precedence) {
  ParserRuleContext *parentContext = _ctx;
  size_t parentState = getState();
  SqlParser::BooleanExpressionContext *_localctx = _tracker.createInstance<BooleanExpressionContext>(_ctx, parentState);
  SqlParser::BooleanExpressionContext *previousContext = _localctx;
  (void)previousContext; // Silence compiler, in case the context is not used by generated code.
  size_t startState = 14;
  enterRecursionRule(_localctx, 14, SqlParser::RuleBooleanExpression, precedence);

    

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    unrollRecursionContexts(parentContext);
  });
  try {
    size_t alt;
    enterOuterAlt(_localctx, 1);
    setState(87);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case SqlParser::T__1:
      case SqlParser::CLP_GET_INT:
      case SqlParser::CLP_GET_FLOAT:
      case SqlParser::CLP_GET_STRING:
      case SqlParser::CLP_GET_BOOL:
      case SqlParser::CLP_GET_JSON_STRING:
      case SqlParser::CLP_WILDCARD_COLUMN:
      case SqlParser::ALL:
      case SqlParser::ANY_VALUE:
      case SqlParser::ARBITRARY:
      case SqlParser::AS:
      case SqlParser::AVG:
      case SqlParser::COUNT:
      case SqlParser::DATE:
      case SqlParser::DISTINCT:
      case SqlParser::FALSE:
      case SqlParser::LIMIT:
      case SqlParser::MAX:
      case SqlParser::MIN:
      case SqlParser::NULL_:
      case SqlParser::SUM:
      case SqlParser::TIMESTAMP:
      case SqlParser::TRUE:
      case SqlParser::PLUS:
      case SqlParser::MINUS:
      case SqlParser::STRING:
      case SqlParser::INTEGER_VALUE:
      case SqlParser::DECIMAL_VALUE:
      case SqlParser::DOUBLE_VALUE:
      case SqlParser::IDENTIFIER:
      case SqlParser::QUOTED_IDENTIFIER:
      case SqlParser::BACKQUOTED_IDENTIFIER: {
        _localctx = _tracker.createInstance<PredicatedContext>(_localctx);
        _ctx = _localctx;
        previousContext = _localctx;

        setState(81);
        antlrcpp::downCast<PredicatedContext *>(_localctx)->valueExpressionContext = valueExpression(0);
        setState(83);
        _errHandler->sync(this);

        switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 8, _ctx)) {
        case 1: {
          setState(82);
          predicate(antlrcpp::downCast<PredicatedContext *>(_localctx)->valueExpressionContext);
          break;
        }

        default:
          break;
        }
        break;
      }

      case SqlParser::NOT: {
        _localctx = _tracker.createInstance<LogicalNotContext>(_localctx);
        _ctx = _localctx;
        previousContext = _localctx;
        setState(85);
        match(SqlParser::NOT);
        setState(86);
        booleanExpression(3);
        break;
      }

    default:
      throw NoViableAltException(this);
    }
    _ctx->stop = _input->LT(-1);
    setState(97);
    _errHandler->sync(this);
    alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 11, _ctx);
    while (alt != 2 && alt != atn::ATN::INVALID_ALT_NUMBER) {
      if (alt == 1) {
        if (!_parseListeners.empty())
          triggerExitRuleEvent();
        previousContext = _localctx;
        setState(95);
        _errHandler->sync(this);
        switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 10, _ctx)) {
        case 1: {
          auto newContext = _tracker.createInstance<LogicalBinaryContext>(_tracker.createInstance<BooleanExpressionContext>(parentContext, parentState));
          _localctx = newContext;
          newContext->left = previousContext;
          pushNewRecursionContext(newContext, startState, RuleBooleanExpression);
          setState(89);

          if (!(precpred(_ctx, 2))) throw FailedPredicateException(this, "precpred(_ctx, 2)");
          setState(90);
          antlrcpp::downCast<LogicalBinaryContext *>(_localctx)->operator_ = match(SqlParser::AND);
          setState(91);
          antlrcpp::downCast<LogicalBinaryContext *>(_localctx)->right = booleanExpression(3);
          break;
        }

        case 2: {
          auto newContext = _tracker.createInstance<LogicalBinaryContext>(_tracker.createInstance<BooleanExpressionContext>(parentContext, parentState));
          _localctx = newContext;
          newContext->left = previousContext;
          pushNewRecursionContext(newContext, startState, RuleBooleanExpression);
          setState(92);

          if (!(precpred(_ctx, 1))) throw FailedPredicateException(this, "precpred(_ctx, 1)");
          setState(93);
          antlrcpp::downCast<LogicalBinaryContext *>(_localctx)->operator_ = match(SqlParser::OR);
          setState(94);
          antlrcpp::downCast<LogicalBinaryContext *>(_localctx)->right = booleanExpression(2);
          break;
        }

        default:
          break;
        } 
      }
      setState(99);
      _errHandler->sync(this);
      alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 11, _ctx);
    }
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }
  return _localctx;
}

//----------------- PredicateContext ------------------------------------------------------------------

SqlParser::PredicateContext::PredicateContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

SqlParser::PredicateContext::PredicateContext(ParserRuleContext *parent, size_t invokingState, antlr4::ParserRuleContext* value)
  : ParserRuleContext(parent, invokingState) {
  this->value = value;
}


size_t SqlParser::PredicateContext::getRuleIndex() const {
  return SqlParser::RulePredicate;
}

void SqlParser::PredicateContext::copyFrom(PredicateContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
  this->value = ctx->value;
}

//----------------- ComparisonContext ------------------------------------------------------------------

SqlParser::ComparisonOperatorContext* SqlParser::ComparisonContext::comparisonOperator() {
  return getRuleContext<SqlParser::ComparisonOperatorContext>(0);
}

SqlParser::ValueExpressionContext* SqlParser::ComparisonContext::valueExpression() {
  return getRuleContext<SqlParser::ValueExpressionContext>(0);
}

SqlParser::ComparisonContext::ComparisonContext(PredicateContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ComparisonContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitComparison(this);
  else
    return visitor->visitChildren(this);
}
//----------------- LikeContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::LikeContext::LIKE() {
  return getToken(SqlParser::LIKE, 0);
}

SqlParser::ValueExpressionContext* SqlParser::LikeContext::valueExpression() {
  return getRuleContext<SqlParser::ValueExpressionContext>(0);
}

tree::TerminalNode* SqlParser::LikeContext::NOT() {
  return getToken(SqlParser::NOT, 0);
}

SqlParser::LikeContext::LikeContext(PredicateContext *ctx) { copyFrom(ctx); }


std::any SqlParser::LikeContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitLike(this);
  else
    return visitor->visitChildren(this);
}
//----------------- InListContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::InListContext::IN() {
  return getToken(SqlParser::IN, 0);
}

std::vector<SqlParser::ExpressionContext *> SqlParser::InListContext::expression() {
  return getRuleContexts<SqlParser::ExpressionContext>();
}

SqlParser::ExpressionContext* SqlParser::InListContext::expression(size_t i) {
  return getRuleContext<SqlParser::ExpressionContext>(i);
}

tree::TerminalNode* SqlParser::InListContext::NOT() {
  return getToken(SqlParser::NOT, 0);
}

SqlParser::InListContext::InListContext(PredicateContext *ctx) { copyFrom(ctx); }


std::any SqlParser::InListContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitInList(this);
  else
    return visitor->visitChildren(this);
}
//----------------- NullPredicateContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::NullPredicateContext::IS() {
  return getToken(SqlParser::IS, 0);
}

tree::TerminalNode* SqlParser::NullPredicateContext::NULL_() {
  return getToken(SqlParser::NULL_, 0);
}

tree::TerminalNode* SqlParser::NullPredicateContext::NOT() {
  return getToken(SqlParser::NOT, 0);
}

SqlParser::NullPredicateContext::NullPredicateContext(PredicateContext *ctx) { copyFrom(ctx); }


std::any SqlParser::NullPredicateContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitNullPredicate(this);
  else
    return visitor->visitChildren(this);
}
//----------------- BetweenContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::BetweenContext::BETWEEN() {
  return getToken(SqlParser::BETWEEN, 0);
}

tree::TerminalNode* SqlParser::BetweenContext::AND() {
  return getToken(SqlParser::AND, 0);
}

std::vector<SqlParser::ValueExpressionContext *> SqlParser::BetweenContext::valueExpression() {
  return getRuleContexts<SqlParser::ValueExpressionContext>();
}

SqlParser::ValueExpressionContext* SqlParser::BetweenContext::valueExpression(size_t i) {
  return getRuleContext<SqlParser::ValueExpressionContext>(i);
}

tree::TerminalNode* SqlParser::BetweenContext::NOT() {
  return getToken(SqlParser::NOT, 0);
}

SqlParser::BetweenContext::BetweenContext(PredicateContext *ctx) { copyFrom(ctx); }


std::any SqlParser::BetweenContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitBetween(this);
  else
    return visitor->visitChildren(this);
}
SqlParser::PredicateContext* SqlParser::predicate(antlr4::ParserRuleContext* value) {
  PredicateContext *_localctx = _tracker.createInstance<PredicateContext>(_ctx, getState(), value);
  enterRule(_localctx, 16, SqlParser::RulePredicate);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    setState(136);
    _errHandler->sync(this);
    switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 17, _ctx)) {
    case 1: {
      _localctx = _tracker.createInstance<SqlParser::ComparisonContext>(_localctx);
      enterOuterAlt(_localctx, 1);
      setState(100);
      comparisonOperator();
      setState(101);
      antlrcpp::downCast<ComparisonContext *>(_localctx)->right = valueExpression(0);
      break;
    }

    case 2: {
      _localctx = _tracker.createInstance<SqlParser::BetweenContext>(_localctx);
      enterOuterAlt(_localctx, 2);
      setState(104);
      _errHandler->sync(this);

      _la = _input->LA(1);
      if (_la == SqlParser::NOT) {
        setState(103);
        match(SqlParser::NOT);
      }
      setState(106);
      match(SqlParser::BETWEEN);
      setState(107);
      antlrcpp::downCast<BetweenContext *>(_localctx)->lower = valueExpression(0);
      setState(108);
      match(SqlParser::AND);
      setState(109);
      antlrcpp::downCast<BetweenContext *>(_localctx)->upper = valueExpression(0);
      break;
    }

    case 3: {
      _localctx = _tracker.createInstance<SqlParser::InListContext>(_localctx);
      enterOuterAlt(_localctx, 3);
      setState(112);
      _errHandler->sync(this);

      _la = _input->LA(1);
      if (_la == SqlParser::NOT) {
        setState(111);
        match(SqlParser::NOT);
      }
      setState(114);
      match(SqlParser::IN);
      setState(115);
      match(SqlParser::T__1);
      setState(116);
      expression();
      setState(121);
      _errHandler->sync(this);
      _la = _input->LA(1);
      while (_la == SqlParser::T__0) {
        setState(117);
        match(SqlParser::T__0);
        setState(118);
        expression();
        setState(123);
        _errHandler->sync(this);
        _la = _input->LA(1);
      }
      setState(124);
      match(SqlParser::T__2);
      break;
    }

    case 4: {
      _localctx = _tracker.createInstance<SqlParser::LikeContext>(_localctx);
      enterOuterAlt(_localctx, 4);
      setState(127);
      _errHandler->sync(this);

      _la = _input->LA(1);
      if (_la == SqlParser::NOT) {
        setState(126);
        match(SqlParser::NOT);
      }
      setState(129);
      match(SqlParser::LIKE);
      setState(130);
      antlrcpp::downCast<LikeContext *>(_localctx)->pattern = valueExpression(0);
      break;
    }

    case 5: {
      _localctx = _tracker.createInstance<SqlParser::NullPredicateContext>(_localctx);
      enterOuterAlt(_localctx, 5);
      setState(131);
      match(SqlParser::IS);
      setState(133);
      _errHandler->sync(this);

      _la = _input->LA(1);
      if (_la == SqlParser::NOT) {
        setState(132);
        match(SqlParser::NOT);
      }
      setState(135);
      match(SqlParser::NULL_);
      break;
    }

    default:
      break;
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- ValueExpressionContext ------------------------------------------------------------------

SqlParser::ValueExpressionContext::ValueExpressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::ValueExpressionContext::getRuleIndex() const {
  return SqlParser::RuleValueExpression;
}

void SqlParser::ValueExpressionContext::copyFrom(ValueExpressionContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- ValueExpressionDefaultContext ------------------------------------------------------------------

SqlParser::PrimaryExpressionContext* SqlParser::ValueExpressionDefaultContext::primaryExpression() {
  return getRuleContext<SqlParser::PrimaryExpressionContext>(0);
}

SqlParser::ValueExpressionDefaultContext::ValueExpressionDefaultContext(ValueExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ValueExpressionDefaultContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitValueExpressionDefault(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ArithmeticBinaryContext ------------------------------------------------------------------

std::vector<SqlParser::ValueExpressionContext *> SqlParser::ArithmeticBinaryContext::valueExpression() {
  return getRuleContexts<SqlParser::ValueExpressionContext>();
}

SqlParser::ValueExpressionContext* SqlParser::ArithmeticBinaryContext::valueExpression(size_t i) {
  return getRuleContext<SqlParser::ValueExpressionContext>(i);
}

tree::TerminalNode* SqlParser::ArithmeticBinaryContext::ASTERISK() {
  return getToken(SqlParser::ASTERISK, 0);
}

tree::TerminalNode* SqlParser::ArithmeticBinaryContext::SLASH() {
  return getToken(SqlParser::SLASH, 0);
}

tree::TerminalNode* SqlParser::ArithmeticBinaryContext::PERCENT() {
  return getToken(SqlParser::PERCENT, 0);
}

tree::TerminalNode* SqlParser::ArithmeticBinaryContext::PLUS() {
  return getToken(SqlParser::PLUS, 0);
}

tree::TerminalNode* SqlParser::ArithmeticBinaryContext::MINUS() {
  return getToken(SqlParser::MINUS, 0);
}

SqlParser::ArithmeticBinaryContext::ArithmeticBinaryContext(ValueExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ArithmeticBinaryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitArithmeticBinary(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ArithmeticUnaryContext ------------------------------------------------------------------

SqlParser::ValueExpressionContext* SqlParser::ArithmeticUnaryContext::valueExpression() {
  return getRuleContext<SqlParser::ValueExpressionContext>(0);
}

tree::TerminalNode* SqlParser::ArithmeticUnaryContext::MINUS() {
  return getToken(SqlParser::MINUS, 0);
}

tree::TerminalNode* SqlParser::ArithmeticUnaryContext::PLUS() {
  return getToken(SqlParser::PLUS, 0);
}

SqlParser::ArithmeticUnaryContext::ArithmeticUnaryContext(ValueExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ArithmeticUnaryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitArithmeticUnary(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::ValueExpressionContext* SqlParser::valueExpression() {
   return valueExpression(0);
}

SqlParser::ValueExpressionContext* SqlParser::valueExpression(int precedence) {
  ParserRuleContext *parentContext = _ctx;
  size_t parentState = getState();
  SqlParser::ValueExpressionContext *_localctx = _tracker.createInstance<ValueExpressionContext>(_ctx, parentState);
  SqlParser::ValueExpressionContext *previousContext = _localctx;
  (void)previousContext; // Silence compiler, in case the context is not used by generated code.
  size_t startState = 18;
  enterRecursionRule(_localctx, 18, SqlParser::RuleValueExpression, precedence);

    size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    unrollRecursionContexts(parentContext);
  });
  try {
    size_t alt;
    enterOuterAlt(_localctx, 1);
    setState(142);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case SqlParser::T__1:
      case SqlParser::CLP_GET_INT:
      case SqlParser::CLP_GET_FLOAT:
      case SqlParser::CLP_GET_STRING:
      case SqlParser::CLP_GET_BOOL:
      case SqlParser::CLP_GET_JSON_STRING:
      case SqlParser::CLP_WILDCARD_COLUMN:
      case SqlParser::ALL:
      case SqlParser::ANY_VALUE:
      case SqlParser::ARBITRARY:
      case SqlParser::AS:
      case SqlParser::AVG:
      case SqlParser::COUNT:
      case SqlParser::DATE:
      case SqlParser::DISTINCT:
      case SqlParser::FALSE:
      case SqlParser::LIMIT:
      case SqlParser::MAX:
      case SqlParser::MIN:
      case SqlParser::NULL_:
      case SqlParser::SUM:
      case SqlParser::TIMESTAMP:
      case SqlParser::TRUE:
      case SqlParser::STRING:
      case SqlParser::INTEGER_VALUE:
      case SqlParser::DECIMAL_VALUE:
      case SqlParser::DOUBLE_VALUE:
      case SqlParser::IDENTIFIER:
      case SqlParser::QUOTED_IDENTIFIER:
      case SqlParser::BACKQUOTED_IDENTIFIER: {
        _localctx = _tracker.createInstance<ValueExpressionDefaultContext>(_localctx);
        _ctx = _localctx;
        previousContext = _localctx;

        setState(139);
        primaryExpression(0);
        break;
      }

      case SqlParser::PLUS:
      case SqlParser::MINUS: {
        _localctx = _tracker.createInstance<ArithmeticUnaryContext>(_localctx);
        _ctx = _localctx;
        previousContext = _localctx;
        setState(140);
        antlrcpp::downCast<ArithmeticUnaryContext *>(_localctx)->operator_ = _input->LT(1);
        _la = _input->LA(1);
        if (!(_la == SqlParser::PLUS

        || _la == SqlParser::MINUS)) {
          antlrcpp::downCast<ArithmeticUnaryContext *>(_localctx)->operator_ = _errHandler->recoverInline(this);
        }
        else {
          _errHandler->reportMatch(this);
          consume();
        }
        setState(141);
        valueExpression(3);
        break;
      }

    default:
      throw NoViableAltException(this);
    }
    _ctx->stop = _input->LT(-1);
    setState(152);
    _errHandler->sync(this);
    alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 20, _ctx);
    while (alt != 2 && alt != atn::ATN::INVALID_ALT_NUMBER) {
      if (alt == 1) {
        if (!_parseListeners.empty())
          triggerExitRuleEvent();
        previousContext = _localctx;
        setState(150);
        _errHandler->sync(this);
        switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 19, _ctx)) {
        case 1: {
          auto newContext = _tracker.createInstance<ArithmeticBinaryContext>(_tracker.createInstance<ValueExpressionContext>(parentContext, parentState));
          _localctx = newContext;
          newContext->left = previousContext;
          pushNewRecursionContext(newContext, startState, RuleValueExpression);
          setState(144);

          if (!(precpred(_ctx, 2))) throw FailedPredicateException(this, "precpred(_ctx, 2)");
          setState(145);
          antlrcpp::downCast<ArithmeticBinaryContext *>(_localctx)->operator_ = _input->LT(1);
          _la = _input->LA(1);
          if (!((((_la & ~ 0x3fULL) == 0) &&
            ((1ULL << _la) & 246290604621824) != 0))) {
            antlrcpp::downCast<ArithmeticBinaryContext *>(_localctx)->operator_ = _errHandler->recoverInline(this);
          }
          else {
            _errHandler->reportMatch(this);
            consume();
          }
          setState(146);
          antlrcpp::downCast<ArithmeticBinaryContext *>(_localctx)->right = valueExpression(3);
          break;
        }

        case 2: {
          auto newContext = _tracker.createInstance<ArithmeticBinaryContext>(_tracker.createInstance<ValueExpressionContext>(parentContext, parentState));
          _localctx = newContext;
          newContext->left = previousContext;
          pushNewRecursionContext(newContext, startState, RuleValueExpression);
          setState(147);

          if (!(precpred(_ctx, 1))) throw FailedPredicateException(this, "precpred(_ctx, 1)");
          setState(148);
          antlrcpp::downCast<ArithmeticBinaryContext *>(_localctx)->operator_ = _input->LT(1);
          _la = _input->LA(1);
          if (!(_la == SqlParser::PLUS

          || _la == SqlParser::MINUS)) {
            antlrcpp::downCast<ArithmeticBinaryContext *>(_localctx)->operator_ = _errHandler->recoverInline(this);
          }
          else {
            _errHandler->reportMatch(this);
            consume();
          }
          setState(149);
          antlrcpp::downCast<ArithmeticBinaryContext *>(_localctx)->right = valueExpression(2);
          break;
        }

        default:
          break;
        } 
      }
      setState(154);
      _errHandler->sync(this);
      alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 20, _ctx);
    }
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }
  return _localctx;
}

//----------------- PrimaryExpressionContext ------------------------------------------------------------------

SqlParser::PrimaryExpressionContext::PrimaryExpressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::PrimaryExpressionContext::getRuleIndex() const {
  return SqlParser::RulePrimaryExpression;
}

void SqlParser::PrimaryExpressionContext::copyFrom(PrimaryExpressionContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- TimestampLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::TimestampLiteralContext::TIMESTAMP() {
  return getToken(SqlParser::TIMESTAMP, 0);
}

SqlParser::StringContext* SqlParser::TimestampLiteralContext::string() {
  return getRuleContext<SqlParser::StringContext>(0);
}

SqlParser::TimestampLiteralContext::TimestampLiteralContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::TimestampLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitTimestampLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- DereferenceContext ------------------------------------------------------------------

SqlParser::PrimaryExpressionContext* SqlParser::DereferenceContext::primaryExpression() {
  return getRuleContext<SqlParser::PrimaryExpressionContext>(0);
}

SqlParser::IdentifierContext* SqlParser::DereferenceContext::identifier() {
  return getRuleContext<SqlParser::IdentifierContext>(0);
}

SqlParser::DereferenceContext::DereferenceContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::DereferenceContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitDereference(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggMaxContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggMaxContext::MAX() {
  return getToken(SqlParser::MAX, 0);
}

SqlParser::ExpressionContext* SqlParser::AggMaxContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::AggMaxContext::AggMaxContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggMaxContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggMax(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ColumnReferenceContext ------------------------------------------------------------------

SqlParser::IdentifierContext* SqlParser::ColumnReferenceContext::identifier() {
  return getRuleContext<SqlParser::IdentifierContext>(0);
}

SqlParser::ColumnReferenceContext::ColumnReferenceContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ColumnReferenceContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitColumnReference(this);
  else
    return visitor->visitChildren(this);
}
//----------------- NullLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::NullLiteralContext::NULL_() {
  return getToken(SqlParser::NULL_, 0);
}

SqlParser::NullLiteralContext::NullLiteralContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::NullLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitNullLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggCountStarContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggCountStarContext::COUNT() {
  return getToken(SqlParser::COUNT, 0);
}

tree::TerminalNode* SqlParser::AggCountStarContext::ASTERISK() {
  return getToken(SqlParser::ASTERISK, 0);
}

SqlParser::AggCountStarContext::AggCountStarContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggCountStarContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggCountStar(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ClpGetBoolContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::ClpGetBoolContext::CLP_GET_BOOL() {
  return getToken(SqlParser::CLP_GET_BOOL, 0);
}

SqlParser::ExpressionContext* SqlParser::ClpGetBoolContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::ClpGetBoolContext::ClpGetBoolContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ClpGetBoolContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitClpGetBool(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ClpGetFloatContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::ClpGetFloatContext::CLP_GET_FLOAT() {
  return getToken(SqlParser::CLP_GET_FLOAT, 0);
}

SqlParser::ExpressionContext* SqlParser::ClpGetFloatContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::ClpGetFloatContext::ClpGetFloatContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ClpGetFloatContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitClpGetFloat(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggSumContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggSumContext::SUM() {
  return getToken(SqlParser::SUM, 0);
}

SqlParser::ExpressionContext* SqlParser::AggSumContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::AggSumContext::AggSumContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggSumContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggSum(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ClpGetStringContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::ClpGetStringContext::CLP_GET_STRING() {
  return getToken(SqlParser::CLP_GET_STRING, 0);
}

SqlParser::ExpressionContext* SqlParser::ClpGetStringContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::ClpGetStringContext::ClpGetStringContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ClpGetStringContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitClpGetString(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggMinContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggMinContext::MIN() {
  return getToken(SqlParser::MIN, 0);
}

SqlParser::ExpressionContext* SqlParser::AggMinContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::AggMinContext::AggMinContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggMinContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggMin(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggAvgContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggAvgContext::AVG() {
  return getToken(SqlParser::AVG, 0);
}

SqlParser::ExpressionContext* SqlParser::AggAvgContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::AggAvgContext::AggAvgContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggAvgContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggAvg(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ParenthesizedExpressionContext ------------------------------------------------------------------

SqlParser::ExpressionContext* SqlParser::ParenthesizedExpressionContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::ParenthesizedExpressionContext::ParenthesizedExpressionContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ParenthesizedExpressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitParenthesizedExpression(this);
  else
    return visitor->visitChildren(this);
}
//----------------- StringLiteralContext ------------------------------------------------------------------

SqlParser::StringContext* SqlParser::StringLiteralContext::string() {
  return getRuleContext<SqlParser::StringContext>(0);
}

SqlParser::StringLiteralContext::StringLiteralContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::StringLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitStringLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggCountContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggCountContext::COUNT() {
  return getToken(SqlParser::COUNT, 0);
}

SqlParser::ExpressionContext* SqlParser::AggCountContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::AggCountContext::AggCountContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggCountContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggCount(this);
  else
    return visitor->visitChildren(this);
}
//----------------- DateLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::DateLiteralContext::DATE() {
  return getToken(SqlParser::DATE, 0);
}

SqlParser::StringContext* SqlParser::DateLiteralContext::string() {
  return getRuleContext<SqlParser::StringContext>(0);
}

SqlParser::DateLiteralContext::DateLiteralContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::DateLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitDateLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ClpGetJsonStringContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::ClpGetJsonStringContext::CLP_GET_JSON_STRING() {
  return getToken(SqlParser::CLP_GET_JSON_STRING, 0);
}

SqlParser::ExpressionContext* SqlParser::ClpGetJsonStringContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::ClpGetJsonStringContext::ClpGetJsonStringContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ClpGetJsonStringContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitClpGetJsonString(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ClpGetIntContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::ClpGetIntContext::CLP_GET_INT() {
  return getToken(SqlParser::CLP_GET_INT, 0);
}

SqlParser::ExpressionContext* SqlParser::ClpGetIntContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

SqlParser::ClpGetIntContext::ClpGetIntContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ClpGetIntContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitClpGetInt(this);
  else
    return visitor->visitChildren(this);
}
//----------------- ClpWildcardColumnContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::ClpWildcardColumnContext::CLP_WILDCARD_COLUMN() {
  return getToken(SqlParser::CLP_WILDCARD_COLUMN, 0);
}

SqlParser::ClpWildcardColumnContext::ClpWildcardColumnContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::ClpWildcardColumnContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitClpWildcardColumn(this);
  else
    return visitor->visitChildren(this);
}
//----------------- AggArbitraryContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::AggArbitraryContext::ARBITRARY() {
  return getToken(SqlParser::ARBITRARY, 0);
}

SqlParser::ExpressionContext* SqlParser::AggArbitraryContext::expression() {
  return getRuleContext<SqlParser::ExpressionContext>(0);
}

tree::TerminalNode* SqlParser::AggArbitraryContext::ANY_VALUE() {
  return getToken(SqlParser::ANY_VALUE, 0);
}

SqlParser::AggArbitraryContext::AggArbitraryContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::AggArbitraryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitAggArbitrary(this);
  else
    return visitor->visitChildren(this);
}
//----------------- NumericLiteralContext ------------------------------------------------------------------

SqlParser::NumberContext* SqlParser::NumericLiteralContext::number() {
  return getRuleContext<SqlParser::NumberContext>(0);
}

SqlParser::NumericLiteralContext::NumericLiteralContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::NumericLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitNumericLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- BooleanLiteralContext ------------------------------------------------------------------

SqlParser::BooleanValueContext* SqlParser::BooleanLiteralContext::booleanValue() {
  return getRuleContext<SqlParser::BooleanValueContext>(0);
}

SqlParser::BooleanLiteralContext::BooleanLiteralContext(PrimaryExpressionContext *ctx) { copyFrom(ctx); }


std::any SqlParser::BooleanLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitBooleanLiteral(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::PrimaryExpressionContext* SqlParser::primaryExpression() {
   return primaryExpression(0);
}

SqlParser::PrimaryExpressionContext* SqlParser::primaryExpression(int precedence) {
  ParserRuleContext *parentContext = _ctx;
  size_t parentState = getState();
  SqlParser::PrimaryExpressionContext *_localctx = _tracker.createInstance<PrimaryExpressionContext>(_ctx, parentState);
  SqlParser::PrimaryExpressionContext *previousContext = _localctx;
  (void)previousContext; // Silence compiler, in case the context is not used by generated code.
  size_t startState = 20;
  enterRecursionRule(_localctx, 20, SqlParser::RulePrimaryExpression, precedence);

    size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    unrollRecursionContexts(parentContext);
  });
  try {
    size_t alt;
    enterOuterAlt(_localctx, 1);
    setState(237);
    _errHandler->sync(this);
    switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 22, _ctx)) {
    case 1: {
      _localctx = _tracker.createInstance<NullLiteralContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;

      setState(156);
      match(SqlParser::NULL_);
      break;
    }

    case 2: {
      _localctx = _tracker.createInstance<NumericLiteralContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(157);
      number();
      break;
    }

    case 3: {
      _localctx = _tracker.createInstance<BooleanLiteralContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(158);
      booleanValue();
      break;
    }

    case 4: {
      _localctx = _tracker.createInstance<StringLiteralContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(159);
      string();
      break;
    }

    case 5: {
      _localctx = _tracker.createInstance<TimestampLiteralContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(160);
      match(SqlParser::TIMESTAMP);
      setState(161);
      string();
      break;
    }

    case 6: {
      _localctx = _tracker.createInstance<DateLiteralContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(162);
      match(SqlParser::DATE);
      setState(163);
      string();
      break;
    }

    case 7: {
      _localctx = _tracker.createInstance<ClpGetIntContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(164);
      match(SqlParser::CLP_GET_INT);
      setState(165);
      match(SqlParser::T__1);
      setState(166);
      expression();
      setState(167);
      match(SqlParser::T__2);
      break;
    }

    case 8: {
      _localctx = _tracker.createInstance<ClpGetFloatContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(169);
      match(SqlParser::CLP_GET_FLOAT);
      setState(170);
      match(SqlParser::T__1);
      setState(171);
      expression();
      setState(172);
      match(SqlParser::T__2);
      break;
    }

    case 9: {
      _localctx = _tracker.createInstance<ClpGetStringContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(174);
      match(SqlParser::CLP_GET_STRING);
      setState(175);
      match(SqlParser::T__1);
      setState(176);
      expression();
      setState(177);
      match(SqlParser::T__2);
      break;
    }

    case 10: {
      _localctx = _tracker.createInstance<ClpGetBoolContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(179);
      match(SqlParser::CLP_GET_BOOL);
      setState(180);
      match(SqlParser::T__1);
      setState(181);
      expression();
      setState(182);
      match(SqlParser::T__2);
      break;
    }

    case 11: {
      _localctx = _tracker.createInstance<ClpGetJsonStringContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(184);
      match(SqlParser::CLP_GET_JSON_STRING);
      setState(185);
      match(SqlParser::T__1);
      setState(187);
      _errHandler->sync(this);

      _la = _input->LA(1);
      if ((((_la & ~ 0x3fULL) == 0) &&
        ((1ULL << _la) & 35773772535295972) != 0)) {
        setState(186);
        expression();
      }
      setState(189);
      match(SqlParser::T__2);
      break;
    }

    case 12: {
      _localctx = _tracker.createInstance<ClpWildcardColumnContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(190);
      match(SqlParser::CLP_WILDCARD_COLUMN);
      setState(191);
      match(SqlParser::T__1);
      setState(192);
      match(SqlParser::T__2);
      break;
    }

    case 13: {
      _localctx = _tracker.createInstance<AggCountStarContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(193);
      match(SqlParser::COUNT);
      setState(194);
      match(SqlParser::T__1);
      setState(195);
      match(SqlParser::ASTERISK);
      setState(196);
      match(SqlParser::T__2);
      break;
    }

    case 14: {
      _localctx = _tracker.createInstance<AggCountContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(197);
      match(SqlParser::COUNT);
      setState(198);
      match(SqlParser::T__1);
      setState(199);
      expression();
      setState(200);
      match(SqlParser::T__2);
      break;
    }

    case 15: {
      _localctx = _tracker.createInstance<AggMinContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(202);
      match(SqlParser::MIN);
      setState(203);
      match(SqlParser::T__1);
      setState(204);
      expression();
      setState(205);
      match(SqlParser::T__2);
      break;
    }

    case 16: {
      _localctx = _tracker.createInstance<AggMaxContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(207);
      match(SqlParser::MAX);
      setState(208);
      match(SqlParser::T__1);
      setState(209);
      expression();
      setState(210);
      match(SqlParser::T__2);
      break;
    }

    case 17: {
      _localctx = _tracker.createInstance<AggSumContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(212);
      match(SqlParser::SUM);
      setState(213);
      match(SqlParser::T__1);
      setState(214);
      expression();
      setState(215);
      match(SqlParser::T__2);
      break;
    }

    case 18: {
      _localctx = _tracker.createInstance<AggAvgContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(217);
      match(SqlParser::AVG);
      setState(218);
      match(SqlParser::T__1);
      setState(219);
      expression();
      setState(220);
      match(SqlParser::T__2);
      break;
    }

    case 19: {
      _localctx = _tracker.createInstance<AggArbitraryContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(222);
      match(SqlParser::ARBITRARY);
      setState(223);
      match(SqlParser::T__1);
      setState(224);
      expression();
      setState(225);
      match(SqlParser::T__2);
      break;
    }

    case 20: {
      _localctx = _tracker.createInstance<AggArbitraryContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(227);
      match(SqlParser::ANY_VALUE);
      setState(228);
      match(SqlParser::T__1);
      setState(229);
      expression();
      setState(230);
      match(SqlParser::T__2);
      break;
    }

    case 21: {
      _localctx = _tracker.createInstance<ColumnReferenceContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(232);
      identifier();
      break;
    }

    case 22: {
      _localctx = _tracker.createInstance<ParenthesizedExpressionContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(233);
      match(SqlParser::T__1);
      setState(234);
      expression();
      setState(235);
      match(SqlParser::T__2);
      break;
    }

    default:
      break;
    }
    _ctx->stop = _input->LT(-1);
    setState(244);
    _errHandler->sync(this);
    alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 23, _ctx);
    while (alt != 2 && alt != atn::ATN::INVALID_ALT_NUMBER) {
      if (alt == 1) {
        if (!_parseListeners.empty())
          triggerExitRuleEvent();
        previousContext = _localctx;
        auto newContext = _tracker.createInstance<DereferenceContext>(_tracker.createInstance<PrimaryExpressionContext>(parentContext, parentState));
        _localctx = newContext;
        newContext->base = previousContext;
        pushNewRecursionContext(newContext, startState, RulePrimaryExpression);
        setState(239);

        if (!(precpred(_ctx, 2))) throw FailedPredicateException(this, "precpred(_ctx, 2)");
        setState(240);
        match(SqlParser::T__3);
        setState(241);
        antlrcpp::downCast<DereferenceContext *>(_localctx)->fieldName = identifier(); 
      }
      setState(246);
      _errHandler->sync(this);
      alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 23, _ctx);
    }
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }
  return _localctx;
}

//----------------- QualifiedNameContext ------------------------------------------------------------------

SqlParser::QualifiedNameContext::QualifiedNameContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

std::vector<SqlParser::IdentifierContext *> SqlParser::QualifiedNameContext::identifier() {
  return getRuleContexts<SqlParser::IdentifierContext>();
}

SqlParser::IdentifierContext* SqlParser::QualifiedNameContext::identifier(size_t i) {
  return getRuleContext<SqlParser::IdentifierContext>(i);
}


size_t SqlParser::QualifiedNameContext::getRuleIndex() const {
  return SqlParser::RuleQualifiedName;
}


std::any SqlParser::QualifiedNameContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitQualifiedName(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::QualifiedNameContext* SqlParser::qualifiedName() {
  QualifiedNameContext *_localctx = _tracker.createInstance<QualifiedNameContext>(_ctx, getState());
  enterRule(_localctx, 22, SqlParser::RuleQualifiedName);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(247);
    identifier();
    setState(252);
    _errHandler->sync(this);
    _la = _input->LA(1);
    while (_la == SqlParser::T__3) {
      setState(248);
      match(SqlParser::T__3);
      setState(249);
      identifier();
      setState(254);
      _errHandler->sync(this);
      _la = _input->LA(1);
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- IdentifierContext ------------------------------------------------------------------

SqlParser::IdentifierContext::IdentifierContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::IdentifierContext::getRuleIndex() const {
  return SqlParser::RuleIdentifier;
}

void SqlParser::IdentifierContext::copyFrom(IdentifierContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- BackQuotedIdentifierContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::BackQuotedIdentifierContext::BACKQUOTED_IDENTIFIER() {
  return getToken(SqlParser::BACKQUOTED_IDENTIFIER, 0);
}

SqlParser::BackQuotedIdentifierContext::BackQuotedIdentifierContext(IdentifierContext *ctx) { copyFrom(ctx); }


std::any SqlParser::BackQuotedIdentifierContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitBackQuotedIdentifier(this);
  else
    return visitor->visitChildren(this);
}
//----------------- QuotedIdentifierContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::QuotedIdentifierContext::QUOTED_IDENTIFIER() {
  return getToken(SqlParser::QUOTED_IDENTIFIER, 0);
}

SqlParser::QuotedIdentifierContext::QuotedIdentifierContext(IdentifierContext *ctx) { copyFrom(ctx); }


std::any SqlParser::QuotedIdentifierContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitQuotedIdentifier(this);
  else
    return visitor->visitChildren(this);
}
//----------------- UnquotedIdentifierContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::UnquotedIdentifierContext::IDENTIFIER() {
  return getToken(SqlParser::IDENTIFIER, 0);
}

SqlParser::NonReservedContext* SqlParser::UnquotedIdentifierContext::nonReserved() {
  return getRuleContext<SqlParser::NonReservedContext>(0);
}

SqlParser::UnquotedIdentifierContext::UnquotedIdentifierContext(IdentifierContext *ctx) { copyFrom(ctx); }


std::any SqlParser::UnquotedIdentifierContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitUnquotedIdentifier(this);
  else
    return visitor->visitChildren(this);
}
SqlParser::IdentifierContext* SqlParser::identifier() {
  IdentifierContext *_localctx = _tracker.createInstance<IdentifierContext>(_ctx, getState());
  enterRule(_localctx, 24, SqlParser::RuleIdentifier);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    setState(259);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case SqlParser::IDENTIFIER: {
        _localctx = _tracker.createInstance<SqlParser::UnquotedIdentifierContext>(_localctx);
        enterOuterAlt(_localctx, 1);
        setState(255);
        match(SqlParser::IDENTIFIER);
        break;
      }

      case SqlParser::QUOTED_IDENTIFIER: {
        _localctx = _tracker.createInstance<SqlParser::QuotedIdentifierContext>(_localctx);
        enterOuterAlt(_localctx, 2);
        setState(256);
        match(SqlParser::QUOTED_IDENTIFIER);
        break;
      }

      case SqlParser::BACKQUOTED_IDENTIFIER: {
        _localctx = _tracker.createInstance<SqlParser::BackQuotedIdentifierContext>(_localctx);
        enterOuterAlt(_localctx, 3);
        setState(257);
        match(SqlParser::BACKQUOTED_IDENTIFIER);
        break;
      }

      case SqlParser::ALL:
      case SqlParser::ANY_VALUE:
      case SqlParser::ARBITRARY:
      case SqlParser::AS:
      case SqlParser::AVG:
      case SqlParser::COUNT:
      case SqlParser::DATE:
      case SqlParser::DISTINCT:
      case SqlParser::LIMIT:
      case SqlParser::MAX:
      case SqlParser::MIN:
      case SqlParser::SUM:
      case SqlParser::TIMESTAMP: {
        _localctx = _tracker.createInstance<SqlParser::UnquotedIdentifierContext>(_localctx);
        enterOuterAlt(_localctx, 4);
        setState(258);
        nonReserved();
        break;
      }

    default:
      throw NoViableAltException(this);
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- NumberContext ------------------------------------------------------------------

SqlParser::NumberContext::NumberContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::NumberContext::getRuleIndex() const {
  return SqlParser::RuleNumber;
}

void SqlParser::NumberContext::copyFrom(NumberContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- DecimalLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::DecimalLiteralContext::DECIMAL_VALUE() {
  return getToken(SqlParser::DECIMAL_VALUE, 0);
}

SqlParser::DecimalLiteralContext::DecimalLiteralContext(NumberContext *ctx) { copyFrom(ctx); }


std::any SqlParser::DecimalLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitDecimalLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- DoubleLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::DoubleLiteralContext::DOUBLE_VALUE() {
  return getToken(SqlParser::DOUBLE_VALUE, 0);
}

SqlParser::DoubleLiteralContext::DoubleLiteralContext(NumberContext *ctx) { copyFrom(ctx); }


std::any SqlParser::DoubleLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitDoubleLiteral(this);
  else
    return visitor->visitChildren(this);
}
//----------------- IntegerLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::IntegerLiteralContext::INTEGER_VALUE() {
  return getToken(SqlParser::INTEGER_VALUE, 0);
}

SqlParser::IntegerLiteralContext::IntegerLiteralContext(NumberContext *ctx) { copyFrom(ctx); }


std::any SqlParser::IntegerLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitIntegerLiteral(this);
  else
    return visitor->visitChildren(this);
}
SqlParser::NumberContext* SqlParser::number() {
  NumberContext *_localctx = _tracker.createInstance<NumberContext>(_ctx, getState());
  enterRule(_localctx, 26, SqlParser::RuleNumber);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    setState(264);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case SqlParser::DECIMAL_VALUE: {
        _localctx = _tracker.createInstance<SqlParser::DecimalLiteralContext>(_localctx);
        enterOuterAlt(_localctx, 1);
        setState(261);
        match(SqlParser::DECIMAL_VALUE);
        break;
      }

      case SqlParser::DOUBLE_VALUE: {
        _localctx = _tracker.createInstance<SqlParser::DoubleLiteralContext>(_localctx);
        enterOuterAlt(_localctx, 2);
        setState(262);
        match(SqlParser::DOUBLE_VALUE);
        break;
      }

      case SqlParser::INTEGER_VALUE: {
        _localctx = _tracker.createInstance<SqlParser::IntegerLiteralContext>(_localctx);
        enterOuterAlt(_localctx, 3);
        setState(263);
        match(SqlParser::INTEGER_VALUE);
        break;
      }

    default:
      throw NoViableAltException(this);
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- StringContext ------------------------------------------------------------------

SqlParser::StringContext::StringContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t SqlParser::StringContext::getRuleIndex() const {
  return SqlParser::RuleString;
}

void SqlParser::StringContext::copyFrom(StringContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- BasicStringLiteralContext ------------------------------------------------------------------

tree::TerminalNode* SqlParser::BasicStringLiteralContext::STRING() {
  return getToken(SqlParser::STRING, 0);
}

SqlParser::BasicStringLiteralContext::BasicStringLiteralContext(StringContext *ctx) { copyFrom(ctx); }


std::any SqlParser::BasicStringLiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitBasicStringLiteral(this);
  else
    return visitor->visitChildren(this);
}
SqlParser::StringContext* SqlParser::string() {
  StringContext *_localctx = _tracker.createInstance<StringContext>(_ctx, getState());
  enterRule(_localctx, 28, SqlParser::RuleString);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    _localctx = _tracker.createInstance<SqlParser::BasicStringLiteralContext>(_localctx);
    enterOuterAlt(_localctx, 1);
    setState(266);
    match(SqlParser::STRING);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- BooleanValueContext ------------------------------------------------------------------

SqlParser::BooleanValueContext::BooleanValueContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* SqlParser::BooleanValueContext::TRUE() {
  return getToken(SqlParser::TRUE, 0);
}

tree::TerminalNode* SqlParser::BooleanValueContext::FALSE() {
  return getToken(SqlParser::FALSE, 0);
}


size_t SqlParser::BooleanValueContext::getRuleIndex() const {
  return SqlParser::RuleBooleanValue;
}


std::any SqlParser::BooleanValueContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitBooleanValue(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::BooleanValueContext* SqlParser::booleanValue() {
  BooleanValueContext *_localctx = _tracker.createInstance<BooleanValueContext>(_ctx, getState());
  enterRule(_localctx, 30, SqlParser::RuleBooleanValue);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(268);
    _la = _input->LA(1);
    if (!(_la == SqlParser::FALSE

    || _la == SqlParser::TRUE)) {
    _errHandler->recoverInline(this);
    }
    else {
      _errHandler->reportMatch(this);
      consume();
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- ComparisonOperatorContext ------------------------------------------------------------------

SqlParser::ComparisonOperatorContext::ComparisonOperatorContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* SqlParser::ComparisonOperatorContext::EQ() {
  return getToken(SqlParser::EQ, 0);
}

tree::TerminalNode* SqlParser::ComparisonOperatorContext::NEQ() {
  return getToken(SqlParser::NEQ, 0);
}

tree::TerminalNode* SqlParser::ComparisonOperatorContext::LT() {
  return getToken(SqlParser::LT, 0);
}

tree::TerminalNode* SqlParser::ComparisonOperatorContext::LTE() {
  return getToken(SqlParser::LTE, 0);
}

tree::TerminalNode* SqlParser::ComparisonOperatorContext::GT() {
  return getToken(SqlParser::GT, 0);
}

tree::TerminalNode* SqlParser::ComparisonOperatorContext::GTE() {
  return getToken(SqlParser::GTE, 0);
}


size_t SqlParser::ComparisonOperatorContext::getRuleIndex() const {
  return SqlParser::RuleComparisonOperator;
}


std::any SqlParser::ComparisonOperatorContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitComparisonOperator(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::ComparisonOperatorContext* SqlParser::comparisonOperator() {
  ComparisonOperatorContext *_localctx = _tracker.createInstance<ComparisonOperatorContext>(_ctx, getState());
  enterRule(_localctx, 32, SqlParser::RuleComparisonOperator);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(270);
    _la = _input->LA(1);
    if (!((((_la & ~ 0x3fULL) == 0) &&
      ((1ULL << _la) & 8658654068736) != 0))) {
    _errHandler->recoverInline(this);
    }
    else {
      _errHandler->reportMatch(this);
      consume();
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- NonReservedContext ------------------------------------------------------------------

SqlParser::NonReservedContext::NonReservedContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* SqlParser::NonReservedContext::ALL() {
  return getToken(SqlParser::ALL, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::ANY_VALUE() {
  return getToken(SqlParser::ANY_VALUE, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::ARBITRARY() {
  return getToken(SqlParser::ARBITRARY, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::AS() {
  return getToken(SqlParser::AS, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::AVG() {
  return getToken(SqlParser::AVG, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::COUNT() {
  return getToken(SqlParser::COUNT, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::DATE() {
  return getToken(SqlParser::DATE, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::DISTINCT() {
  return getToken(SqlParser::DISTINCT, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::LIMIT() {
  return getToken(SqlParser::LIMIT, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::MAX() {
  return getToken(SqlParser::MAX, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::MIN() {
  return getToken(SqlParser::MIN, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::SUM() {
  return getToken(SqlParser::SUM, 0);
}

tree::TerminalNode* SqlParser::NonReservedContext::TIMESTAMP() {
  return getToken(SqlParser::TIMESTAMP, 0);
}


size_t SqlParser::NonReservedContext::getRuleIndex() const {
  return SqlParser::RuleNonReserved;
}


std::any SqlParser::NonReservedContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<SqlVisitor*>(visitor))
    return parserVisitor->visitNonReserved(this);
  else
    return visitor->visitChildren(this);
}

SqlParser::NonReservedContext* SqlParser::nonReserved() {
  NonReservedContext *_localctx = _tracker.createInstance<NonReservedContext>(_ctx, getState());
  enterRule(_localctx, 34, SqlParser::RuleNonReserved);
  size_t _la = 0;

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(272);
    _la = _input->LA(1);
    if (!((((_la & ~ 0x3fULL) == 0) &&
      ((1ULL << _la) & 26241525760) != 0))) {
    _errHandler->recoverInline(this);
    }
    else {
      _errHandler->reportMatch(this);
      consume();
    }
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

bool SqlParser::sempred(RuleContext *context, size_t ruleIndex, size_t predicateIndex) {
  switch (ruleIndex) {
    case 7: return booleanExpressionSempred(antlrcpp::downCast<BooleanExpressionContext *>(context), predicateIndex);
    case 9: return valueExpressionSempred(antlrcpp::downCast<ValueExpressionContext *>(context), predicateIndex);
    case 10: return primaryExpressionSempred(antlrcpp::downCast<PrimaryExpressionContext *>(context), predicateIndex);

  default:
    break;
  }
  return true;
}

bool SqlParser::booleanExpressionSempred(BooleanExpressionContext *_localctx, size_t predicateIndex) {
  switch (predicateIndex) {
    case 0: return precpred(_ctx, 2);
    case 1: return precpred(_ctx, 1);

  default:
    break;
  }
  return true;
}

bool SqlParser::valueExpressionSempred(ValueExpressionContext *_localctx, size_t predicateIndex) {
  switch (predicateIndex) {
    case 2: return precpred(_ctx, 2);
    case 3: return precpred(_ctx, 1);

  default:
    break;
  }
  return true;
}

bool SqlParser::primaryExpressionSempred(PrimaryExpressionContext *_localctx, size_t predicateIndex) {
  switch (predicateIndex) {
    case 4: return precpred(_ctx, 2);

  default:
    break;
  }
  return true;
}

void SqlParser::initialize() {
#if ANTLR4_USE_THREAD_LOCAL_CACHE
  sqlParserInitialize();
#else
  ::antlr4::internal::call_once(sqlParserOnceFlag, sqlParserInitialize);
#endif
}
