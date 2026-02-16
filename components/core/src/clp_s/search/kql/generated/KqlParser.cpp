
// Generated from Kql.g4 by ANTLR 4.13.2


#include "KqlVisitor.h"

#include "KqlParser.h"


using namespace antlrcpp;
using namespace clp_s::search::kql::generated;

using namespace antlr4;

namespace {

struct KqlParserStaticData final {
  KqlParserStaticData(std::vector<std::string> ruleNames,
                        std::vector<std::string> literalNames,
                        std::vector<std::string> symbolicNames)
      : ruleNames(std::move(ruleNames)), literalNames(std::move(literalNames)),
        symbolicNames(std::move(symbolicNames)),
        vocabulary(this->literalNames, this->symbolicNames) {}

  KqlParserStaticData(const KqlParserStaticData&) = delete;
  KqlParserStaticData(KqlParserStaticData&&) = delete;
  KqlParserStaticData& operator=(const KqlParserStaticData&) = delete;
  KqlParserStaticData& operator=(KqlParserStaticData&&) = delete;

  std::vector<antlr4::dfa::DFA> decisionToDFA;
  antlr4::atn::PredictionContextCache sharedContextCache;
  const std::vector<std::string> ruleNames;
  const std::vector<std::string> literalNames;
  const std::vector<std::string> symbolicNames;
  const antlr4::dfa::Vocabulary vocabulary;
  antlr4::atn::SerializedATNView serializedATN;
  std::unique_ptr<antlr4::atn::ATN> atn;
};

::antlr4::internal::OnceFlag kqlParserOnceFlag;
#if ANTLR4_USE_THREAD_LOCAL_CACHE
static thread_local
#endif
std::unique_ptr<KqlParserStaticData> kqlParserStaticData = nullptr;

void kqlParserInitialize() {
#if ANTLR4_USE_THREAD_LOCAL_CACHE
  if (kqlParserStaticData != nullptr) {
    return;
  }
#else
  assert(kqlParserStaticData == nullptr);
#endif
  auto staticData = std::make_unique<KqlParserStaticData>(
    std::vector<std::string>{
      "start", "query", "expression", "column_range_expression", "column_value_expression", 
      "column", "value_expression", "list_of_values", "semantic_expression", 
      "timestamp_expression", "literal"
    },
    std::vector<std::string>{
      "", "':'", "'{'", "'}'", "'('", "')'", "'semantic('", "','", "'timestamp('"
    },
    std::vector<std::string>{
      "", "", "", "", "", "", "", "", "", "AND", "OR", "NOT", "QUOTED_STRING", 
      "UNQUOTED_LITERAL", "RANGE_OPERATOR", "SPACE"
    }
  );
  static const int32_t serializedATNSegment[] = {
  	4,1,15,103,2,0,7,0,2,1,7,1,2,2,7,2,2,3,7,3,2,4,7,4,2,5,7,5,2,6,7,6,2,
  	7,7,7,2,8,7,8,2,9,7,9,2,10,7,10,1,0,1,0,1,0,1,1,1,1,1,1,1,1,1,1,1,1,1,
  	1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,3,1,40,8,1,1,1,1,1,1,1,5,1,45,8,1,10,1,
  	12,1,48,9,1,1,2,1,2,1,2,1,2,3,2,54,8,2,1,3,1,3,1,3,1,3,3,3,60,8,3,1,4,
  	1,4,1,4,1,4,1,4,3,4,67,8,4,1,5,1,5,1,6,1,6,1,7,1,7,3,7,75,8,7,1,7,5,7,
  	78,8,7,10,7,12,7,81,9,7,1,7,1,7,1,8,1,8,1,8,1,8,3,8,89,8,8,1,8,1,8,1,
  	9,1,9,1,9,1,9,3,9,97,8,9,1,9,1,9,1,10,1,10,1,10,0,1,2,11,0,2,4,6,8,10,
  	12,14,16,18,20,0,3,1,0,9,10,1,0,9,11,1,0,12,13,105,0,22,1,0,0,0,2,39,
  	1,0,0,0,4,53,1,0,0,0,6,55,1,0,0,0,8,61,1,0,0,0,10,68,1,0,0,0,12,70,1,
  	0,0,0,14,72,1,0,0,0,16,84,1,0,0,0,18,92,1,0,0,0,20,100,1,0,0,0,22,23,
  	3,2,1,0,23,24,5,0,0,1,24,1,1,0,0,0,25,26,6,1,-1,0,26,27,3,10,5,0,27,28,
  	5,1,0,0,28,29,5,2,0,0,29,30,3,2,1,0,30,31,5,3,0,0,31,40,1,0,0,0,32,33,
  	5,4,0,0,33,34,3,2,1,0,34,35,5,5,0,0,35,40,1,0,0,0,36,37,5,11,0,0,37,40,
  	3,2,1,3,38,40,3,4,2,0,39,25,1,0,0,0,39,32,1,0,0,0,39,36,1,0,0,0,39,38,
  	1,0,0,0,40,46,1,0,0,0,41,42,10,2,0,0,42,43,7,0,0,0,43,45,3,2,1,3,44,41,
  	1,0,0,0,45,48,1,0,0,0,46,44,1,0,0,0,46,47,1,0,0,0,47,3,1,0,0,0,48,46,
  	1,0,0,0,49,54,3,6,3,0,50,54,3,8,4,0,51,54,3,16,8,0,52,54,3,12,6,0,53,
  	49,1,0,0,0,53,50,1,0,0,0,53,51,1,0,0,0,53,52,1,0,0,0,54,5,1,0,0,0,55,
  	56,3,10,5,0,56,59,5,14,0,0,57,60,3,18,9,0,58,60,3,20,10,0,59,57,1,0,0,
  	0,59,58,1,0,0,0,60,7,1,0,0,0,61,62,3,10,5,0,62,66,5,1,0,0,63,67,3,14,
  	7,0,64,67,3,18,9,0,65,67,3,20,10,0,66,63,1,0,0,0,66,64,1,0,0,0,66,65,
  	1,0,0,0,67,9,1,0,0,0,68,69,3,20,10,0,69,11,1,0,0,0,70,71,3,20,10,0,71,
  	13,1,0,0,0,72,74,5,4,0,0,73,75,7,1,0,0,74,73,1,0,0,0,74,75,1,0,0,0,75,
  	79,1,0,0,0,76,78,3,20,10,0,77,76,1,0,0,0,78,81,1,0,0,0,79,77,1,0,0,0,
  	79,80,1,0,0,0,80,82,1,0,0,0,81,79,1,0,0,0,82,83,5,5,0,0,83,15,1,0,0,0,
  	84,85,5,6,0,0,85,88,5,12,0,0,86,87,5,7,0,0,87,89,5,13,0,0,88,86,1,0,0,
  	0,88,89,1,0,0,0,89,90,1,0,0,0,90,91,5,5,0,0,91,17,1,0,0,0,92,93,5,8,0,
  	0,93,96,5,12,0,0,94,95,5,7,0,0,95,97,5,12,0,0,96,94,1,0,0,0,96,97,1,0,
  	0,0,97,98,1,0,0,0,98,99,5,5,0,0,99,19,1,0,0,0,100,101,7,2,0,0,101,21,
  	1,0,0,0,9,39,46,53,59,66,74,79,88,96
  };
  staticData->serializedATN = antlr4::atn::SerializedATNView(serializedATNSegment, sizeof(serializedATNSegment) / sizeof(serializedATNSegment[0]));

  antlr4::atn::ATNDeserializer deserializer;
  staticData->atn = deserializer.deserialize(staticData->serializedATN);

  const size_t count = staticData->atn->getNumberOfDecisions();
  staticData->decisionToDFA.reserve(count);
  for (size_t i = 0; i < count; i++) { 
    staticData->decisionToDFA.emplace_back(staticData->atn->getDecisionState(i), i);
  }
  kqlParserStaticData = std::move(staticData);
}

}

KqlParser::KqlParser(TokenStream *input) : KqlParser(input, antlr4::atn::ParserATNSimulatorOptions()) {}

KqlParser::KqlParser(TokenStream *input, const antlr4::atn::ParserATNSimulatorOptions &options) : Parser(input) {
  KqlParser::initialize();
  _interpreter = new atn::ParserATNSimulator(this, *kqlParserStaticData->atn, kqlParserStaticData->decisionToDFA, kqlParserStaticData->sharedContextCache, options);
}

KqlParser::~KqlParser() {
  delete _interpreter;
}

const atn::ATN& KqlParser::getATN() const {
  return *kqlParserStaticData->atn;
}

std::string KqlParser::getGrammarFileName() const {
  return "Kql.g4";
}

const std::vector<std::string>& KqlParser::getRuleNames() const {
  return kqlParserStaticData->ruleNames;
}

const dfa::Vocabulary& KqlParser::getVocabulary() const {
  return kqlParserStaticData->vocabulary;
}

antlr4::atn::SerializedATNView KqlParser::getSerializedATN() const {
  return kqlParserStaticData->serializedATN;
}


//----------------- StartContext ------------------------------------------------------------------

KqlParser::StartContext::StartContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

KqlParser::QueryContext* KqlParser::StartContext::query() {
  return getRuleContext<KqlParser::QueryContext>(0);
}

tree::TerminalNode* KqlParser::StartContext::EOF() {
  return getToken(KqlParser::EOF, 0);
}


size_t KqlParser::StartContext::getRuleIndex() const {
  return KqlParser::RuleStart;
}


std::any KqlParser::StartContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitStart(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::StartContext* KqlParser::start() {
  StartContext *_localctx = _tracker.createInstance<StartContext>(_ctx, getState());
  enterRule(_localctx, 0, KqlParser::RuleStart);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(22);
    query(0);
    setState(23);
    match(KqlParser::EOF);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- QueryContext ------------------------------------------------------------------

KqlParser::QueryContext::QueryContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}


size_t KqlParser::QueryContext::getRuleIndex() const {
  return KqlParser::RuleQuery;
}

void KqlParser::QueryContext::copyFrom(QueryContext *ctx) {
  ParserRuleContext::copyFrom(ctx);
}

//----------------- ExprContext ------------------------------------------------------------------

KqlParser::ExpressionContext* KqlParser::ExprContext::expression() {
  return getRuleContext<KqlParser::ExpressionContext>(0);
}

KqlParser::ExprContext::ExprContext(QueryContext *ctx) { copyFrom(ctx); }


std::any KqlParser::ExprContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitExpr(this);
  else
    return visitor->visitChildren(this);
}
//----------------- NestedQueryContext ------------------------------------------------------------------

KqlParser::ColumnContext* KqlParser::NestedQueryContext::column() {
  return getRuleContext<KqlParser::ColumnContext>(0);
}

KqlParser::QueryContext* KqlParser::NestedQueryContext::query() {
  return getRuleContext<KqlParser::QueryContext>(0);
}

KqlParser::NestedQueryContext::NestedQueryContext(QueryContext *ctx) { copyFrom(ctx); }


std::any KqlParser::NestedQueryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitNestedQuery(this);
  else
    return visitor->visitChildren(this);
}
//----------------- NotQueryContext ------------------------------------------------------------------

tree::TerminalNode* KqlParser::NotQueryContext::NOT() {
  return getToken(KqlParser::NOT, 0);
}

KqlParser::QueryContext* KqlParser::NotQueryContext::query() {
  return getRuleContext<KqlParser::QueryContext>(0);
}

KqlParser::NotQueryContext::NotQueryContext(QueryContext *ctx) { copyFrom(ctx); }


std::any KqlParser::NotQueryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitNotQuery(this);
  else
    return visitor->visitChildren(this);
}
//----------------- SubQueryContext ------------------------------------------------------------------

KqlParser::QueryContext* KqlParser::SubQueryContext::query() {
  return getRuleContext<KqlParser::QueryContext>(0);
}

KqlParser::SubQueryContext::SubQueryContext(QueryContext *ctx) { copyFrom(ctx); }


std::any KqlParser::SubQueryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitSubQuery(this);
  else
    return visitor->visitChildren(this);
}
//----------------- OrAndQueryContext ------------------------------------------------------------------

std::vector<KqlParser::QueryContext *> KqlParser::OrAndQueryContext::query() {
  return getRuleContexts<KqlParser::QueryContext>();
}

KqlParser::QueryContext* KqlParser::OrAndQueryContext::query(size_t i) {
  return getRuleContext<KqlParser::QueryContext>(i);
}

tree::TerminalNode* KqlParser::OrAndQueryContext::OR() {
  return getToken(KqlParser::OR, 0);
}

tree::TerminalNode* KqlParser::OrAndQueryContext::AND() {
  return getToken(KqlParser::AND, 0);
}

KqlParser::OrAndQueryContext::OrAndQueryContext(QueryContext *ctx) { copyFrom(ctx); }


std::any KqlParser::OrAndQueryContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitOrAndQuery(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::QueryContext* KqlParser::query() {
   return query(0);
}

KqlParser::QueryContext* KqlParser::query(int precedence) {
  ParserRuleContext *parentContext = _ctx;
  size_t parentState = getState();
  KqlParser::QueryContext *_localctx = _tracker.createInstance<QueryContext>(_ctx, parentState);
  KqlParser::QueryContext *previousContext = _localctx;
  (void)previousContext; // Silence compiler, in case the context is not used by generated code.
  size_t startState = 2;
  enterRecursionRule(_localctx, 2, KqlParser::RuleQuery, precedence);

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
    setState(39);
    _errHandler->sync(this);
    switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 0, _ctx)) {
    case 1: {
      _localctx = _tracker.createInstance<NestedQueryContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;

      setState(26);
      antlrcpp::downCast<NestedQueryContext *>(_localctx)->col = column();
      setState(27);
      match(KqlParser::T__0);
      setState(28);
      match(KqlParser::T__1);
      setState(29);
      antlrcpp::downCast<NestedQueryContext *>(_localctx)->q = query(0);
      setState(30);
      match(KqlParser::T__2);
      break;
    }

    case 2: {
      _localctx = _tracker.createInstance<SubQueryContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(32);
      match(KqlParser::T__3);
      setState(33);
      antlrcpp::downCast<SubQueryContext *>(_localctx)->q = query(0);
      setState(34);
      match(KqlParser::T__4);
      break;
    }

    case 3: {
      _localctx = _tracker.createInstance<NotQueryContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(36);
      match(KqlParser::NOT);
      setState(37);
      antlrcpp::downCast<NotQueryContext *>(_localctx)->q = query(3);
      break;
    }

    case 4: {
      _localctx = _tracker.createInstance<ExprContext>(_localctx);
      _ctx = _localctx;
      previousContext = _localctx;
      setState(38);
      expression();
      break;
    }

    default:
      break;
    }
    _ctx->stop = _input->LT(-1);
    setState(46);
    _errHandler->sync(this);
    alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 1, _ctx);
    while (alt != 2 && alt != atn::ATN::INVALID_ALT_NUMBER) {
      if (alt == 1) {
        if (!_parseListeners.empty())
          triggerExitRuleEvent();
        previousContext = _localctx;
        auto newContext = _tracker.createInstance<OrAndQueryContext>(_tracker.createInstance<QueryContext>(parentContext, parentState));
        _localctx = newContext;
        newContext->lhs = previousContext;
        pushNewRecursionContext(newContext, startState, RuleQuery);
        setState(41);

        if (!(precpred(_ctx, 2))) throw FailedPredicateException(this, "precpred(_ctx, 2)");
        setState(42);
        antlrcpp::downCast<OrAndQueryContext *>(_localctx)->op = _input->LT(1);
        _la = _input->LA(1);
        if (!(_la == KqlParser::AND

        || _la == KqlParser::OR)) {
          antlrcpp::downCast<OrAndQueryContext *>(_localctx)->op = _errHandler->recoverInline(this);
        }
        else {
          _errHandler->reportMatch(this);
          consume();
        }
        setState(43);
        antlrcpp::downCast<OrAndQueryContext *>(_localctx)->rhs = query(3); 
      }
      setState(48);
      _errHandler->sync(this);
      alt = getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 1, _ctx);
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

KqlParser::ExpressionContext::ExpressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

KqlParser::Column_range_expressionContext* KqlParser::ExpressionContext::column_range_expression() {
  return getRuleContext<KqlParser::Column_range_expressionContext>(0);
}

KqlParser::Column_value_expressionContext* KqlParser::ExpressionContext::column_value_expression() {
  return getRuleContext<KqlParser::Column_value_expressionContext>(0);
}

KqlParser::Semantic_expressionContext* KqlParser::ExpressionContext::semantic_expression() {
  return getRuleContext<KqlParser::Semantic_expressionContext>(0);
}

KqlParser::Value_expressionContext* KqlParser::ExpressionContext::value_expression() {
  return getRuleContext<KqlParser::Value_expressionContext>(0);
}


size_t KqlParser::ExpressionContext::getRuleIndex() const {
  return KqlParser::RuleExpression;
}


std::any KqlParser::ExpressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitExpression(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::ExpressionContext* KqlParser::expression() {
  ExpressionContext *_localctx = _tracker.createInstance<ExpressionContext>(_ctx, getState());
  enterRule(_localctx, 4, KqlParser::RuleExpression);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    setState(53);
    _errHandler->sync(this);
    switch (getInterpreter<atn::ParserATNSimulator>()->adaptivePredict(_input, 2, _ctx)) {
    case 1: {
      enterOuterAlt(_localctx, 1);
      setState(49);
      column_range_expression();
      break;
    }

    case 2: {
      enterOuterAlt(_localctx, 2);
      setState(50);
      column_value_expression();
      break;
    }

    case 3: {
      enterOuterAlt(_localctx, 3);
      setState(51);
      semantic_expression();
      break;
    }

    case 4: {
      enterOuterAlt(_localctx, 4);
      setState(52);
      value_expression();
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

//----------------- Column_range_expressionContext ------------------------------------------------------------------

KqlParser::Column_range_expressionContext::Column_range_expressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* KqlParser::Column_range_expressionContext::RANGE_OPERATOR() {
  return getToken(KqlParser::RANGE_OPERATOR, 0);
}

KqlParser::ColumnContext* KqlParser::Column_range_expressionContext::column() {
  return getRuleContext<KqlParser::ColumnContext>(0);
}

KqlParser::Timestamp_expressionContext* KqlParser::Column_range_expressionContext::timestamp_expression() {
  return getRuleContext<KqlParser::Timestamp_expressionContext>(0);
}

KqlParser::LiteralContext* KqlParser::Column_range_expressionContext::literal() {
  return getRuleContext<KqlParser::LiteralContext>(0);
}


size_t KqlParser::Column_range_expressionContext::getRuleIndex() const {
  return KqlParser::RuleColumn_range_expression;
}


std::any KqlParser::Column_range_expressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitColumn_range_expression(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::Column_range_expressionContext* KqlParser::column_range_expression() {
  Column_range_expressionContext *_localctx = _tracker.createInstance<Column_range_expressionContext>(_ctx, getState());
  enterRule(_localctx, 6, KqlParser::RuleColumn_range_expression);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(55);
    antlrcpp::downCast<Column_range_expressionContext *>(_localctx)->col = column();
    setState(56);
    match(KqlParser::RANGE_OPERATOR);
    setState(59);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case KqlParser::T__7: {
        setState(57);
        antlrcpp::downCast<Column_range_expressionContext *>(_localctx)->timestamp = timestamp_expression();
        break;
      }

      case KqlParser::QUOTED_STRING:
      case KqlParser::UNQUOTED_LITERAL: {
        setState(58);
        antlrcpp::downCast<Column_range_expressionContext *>(_localctx)->lit = literal();
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

//----------------- Column_value_expressionContext ------------------------------------------------------------------

KqlParser::Column_value_expressionContext::Column_value_expressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

KqlParser::ColumnContext* KqlParser::Column_value_expressionContext::column() {
  return getRuleContext<KqlParser::ColumnContext>(0);
}

KqlParser::List_of_valuesContext* KqlParser::Column_value_expressionContext::list_of_values() {
  return getRuleContext<KqlParser::List_of_valuesContext>(0);
}

KqlParser::Timestamp_expressionContext* KqlParser::Column_value_expressionContext::timestamp_expression() {
  return getRuleContext<KqlParser::Timestamp_expressionContext>(0);
}

KqlParser::LiteralContext* KqlParser::Column_value_expressionContext::literal() {
  return getRuleContext<KqlParser::LiteralContext>(0);
}


size_t KqlParser::Column_value_expressionContext::getRuleIndex() const {
  return KqlParser::RuleColumn_value_expression;
}


std::any KqlParser::Column_value_expressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitColumn_value_expression(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::Column_value_expressionContext* KqlParser::column_value_expression() {
  Column_value_expressionContext *_localctx = _tracker.createInstance<Column_value_expressionContext>(_ctx, getState());
  enterRule(_localctx, 8, KqlParser::RuleColumn_value_expression);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(61);
    antlrcpp::downCast<Column_value_expressionContext *>(_localctx)->col = column();
    setState(62);
    match(KqlParser::T__0);
    setState(66);
    _errHandler->sync(this);
    switch (_input->LA(1)) {
      case KqlParser::T__3: {
        setState(63);
        antlrcpp::downCast<Column_value_expressionContext *>(_localctx)->list = list_of_values();
        break;
      }

      case KqlParser::T__7: {
        setState(64);
        antlrcpp::downCast<Column_value_expressionContext *>(_localctx)->timestamp = timestamp_expression();
        break;
      }

      case KqlParser::QUOTED_STRING:
      case KqlParser::UNQUOTED_LITERAL: {
        setState(65);
        antlrcpp::downCast<Column_value_expressionContext *>(_localctx)->lit = literal();
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

//----------------- ColumnContext ------------------------------------------------------------------

KqlParser::ColumnContext::ColumnContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

KqlParser::LiteralContext* KqlParser::ColumnContext::literal() {
  return getRuleContext<KqlParser::LiteralContext>(0);
}


size_t KqlParser::ColumnContext::getRuleIndex() const {
  return KqlParser::RuleColumn;
}


std::any KqlParser::ColumnContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitColumn(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::ColumnContext* KqlParser::column() {
  ColumnContext *_localctx = _tracker.createInstance<ColumnContext>(_ctx, getState());
  enterRule(_localctx, 10, KqlParser::RuleColumn);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(68);
    literal();
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- Value_expressionContext ------------------------------------------------------------------

KqlParser::Value_expressionContext::Value_expressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

KqlParser::LiteralContext* KqlParser::Value_expressionContext::literal() {
  return getRuleContext<KqlParser::LiteralContext>(0);
}


size_t KqlParser::Value_expressionContext::getRuleIndex() const {
  return KqlParser::RuleValue_expression;
}


std::any KqlParser::Value_expressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitValue_expression(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::Value_expressionContext* KqlParser::value_expression() {
  Value_expressionContext *_localctx = _tracker.createInstance<Value_expressionContext>(_ctx, getState());
  enterRule(_localctx, 12, KqlParser::RuleValue_expression);

#if __cplusplus > 201703L
  auto onExit = finally([=, this] {
#else
  auto onExit = finally([=] {
#endif
    exitRule();
  });
  try {
    enterOuterAlt(_localctx, 1);
    setState(70);
    literal();
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- List_of_valuesContext ------------------------------------------------------------------

KqlParser::List_of_valuesContext::List_of_valuesContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

std::vector<KqlParser::LiteralContext *> KqlParser::List_of_valuesContext::literal() {
  return getRuleContexts<KqlParser::LiteralContext>();
}

KqlParser::LiteralContext* KqlParser::List_of_valuesContext::literal(size_t i) {
  return getRuleContext<KqlParser::LiteralContext>(i);
}

tree::TerminalNode* KqlParser::List_of_valuesContext::AND() {
  return getToken(KqlParser::AND, 0);
}

tree::TerminalNode* KqlParser::List_of_valuesContext::OR() {
  return getToken(KqlParser::OR, 0);
}

tree::TerminalNode* KqlParser::List_of_valuesContext::NOT() {
  return getToken(KqlParser::NOT, 0);
}


size_t KqlParser::List_of_valuesContext::getRuleIndex() const {
  return KqlParser::RuleList_of_values;
}


std::any KqlParser::List_of_valuesContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitList_of_values(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::List_of_valuesContext* KqlParser::list_of_values() {
  List_of_valuesContext *_localctx = _tracker.createInstance<List_of_valuesContext>(_ctx, getState());
  enterRule(_localctx, 14, KqlParser::RuleList_of_values);
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
    setState(72);
    match(KqlParser::T__3);
    setState(74);
    _errHandler->sync(this);

    _la = _input->LA(1);
    if ((((_la & ~ 0x3fULL) == 0) &&
      ((1ULL << _la) & 3584) != 0)) {
      setState(73);
      antlrcpp::downCast<List_of_valuesContext *>(_localctx)->condition = _input->LT(1);
      _la = _input->LA(1);
      if (!((((_la & ~ 0x3fULL) == 0) &&
        ((1ULL << _la) & 3584) != 0))) {
        antlrcpp::downCast<List_of_valuesContext *>(_localctx)->condition = _errHandler->recoverInline(this);
      }
      else {
        _errHandler->reportMatch(this);
        consume();
      }
    }
    setState(79);
    _errHandler->sync(this);
    _la = _input->LA(1);
    while (_la == KqlParser::QUOTED_STRING

    || _la == KqlParser::UNQUOTED_LITERAL) {
      setState(76);
      antlrcpp::downCast<List_of_valuesContext *>(_localctx)->literalContext = literal();
      antlrcpp::downCast<List_of_valuesContext *>(_localctx)->literals.push_back(antlrcpp::downCast<List_of_valuesContext *>(_localctx)->literalContext);
      setState(81);
      _errHandler->sync(this);
      _la = _input->LA(1);
    }
    setState(82);
    match(KqlParser::T__4);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- Semantic_expressionContext ------------------------------------------------------------------

KqlParser::Semantic_expressionContext::Semantic_expressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* KqlParser::Semantic_expressionContext::QUOTED_STRING() {
  return getToken(KqlParser::QUOTED_STRING, 0);
}

tree::TerminalNode* KqlParser::Semantic_expressionContext::UNQUOTED_LITERAL() {
  return getToken(KqlParser::UNQUOTED_LITERAL, 0);
}


size_t KqlParser::Semantic_expressionContext::getRuleIndex() const {
  return KqlParser::RuleSemantic_expression;
}


std::any KqlParser::Semantic_expressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitSemantic_expression(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::Semantic_expressionContext* KqlParser::semantic_expression() {
  Semantic_expressionContext *_localctx = _tracker.createInstance<Semantic_expressionContext>(_ctx, getState());
  enterRule(_localctx, 16, KqlParser::RuleSemantic_expression);
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
    setState(84);
    match(KqlParser::T__5);
    setState(85);
    antlrcpp::downCast<Semantic_expressionContext *>(_localctx)->query_text = match(KqlParser::QUOTED_STRING);
    setState(88);
    _errHandler->sync(this);

    _la = _input->LA(1);
    if (_la == KqlParser::T__6) {
      setState(86);
      match(KqlParser::T__6);
      setState(87);
      antlrcpp::downCast<Semantic_expressionContext *>(_localctx)->top_k = match(KqlParser::UNQUOTED_LITERAL);
    }
    setState(90);
    match(KqlParser::T__4);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- Timestamp_expressionContext ------------------------------------------------------------------

KqlParser::Timestamp_expressionContext::Timestamp_expressionContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

std::vector<tree::TerminalNode *> KqlParser::Timestamp_expressionContext::QUOTED_STRING() {
  return getTokens(KqlParser::QUOTED_STRING);
}

tree::TerminalNode* KqlParser::Timestamp_expressionContext::QUOTED_STRING(size_t i) {
  return getToken(KqlParser::QUOTED_STRING, i);
}


size_t KqlParser::Timestamp_expressionContext::getRuleIndex() const {
  return KqlParser::RuleTimestamp_expression;
}


std::any KqlParser::Timestamp_expressionContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitTimestamp_expression(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::Timestamp_expressionContext* KqlParser::timestamp_expression() {
  Timestamp_expressionContext *_localctx = _tracker.createInstance<Timestamp_expressionContext>(_ctx, getState());
  enterRule(_localctx, 18, KqlParser::RuleTimestamp_expression);
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
    setState(92);
    match(KqlParser::T__7);
    setState(93);
    antlrcpp::downCast<Timestamp_expressionContext *>(_localctx)->timestamp = match(KqlParser::QUOTED_STRING);
    setState(96);
    _errHandler->sync(this);

    _la = _input->LA(1);
    if (_la == KqlParser::T__6) {
      setState(94);
      match(KqlParser::T__6);
      setState(95);
      antlrcpp::downCast<Timestamp_expressionContext *>(_localctx)->pattern = match(KqlParser::QUOTED_STRING);
    }
    setState(98);
    match(KqlParser::T__4);
   
  }
  catch (RecognitionException &e) {
    _errHandler->reportError(this, e);
    _localctx->exception = std::current_exception();
    _errHandler->recover(this, _localctx->exception);
  }

  return _localctx;
}

//----------------- LiteralContext ------------------------------------------------------------------

KqlParser::LiteralContext::LiteralContext(ParserRuleContext *parent, size_t invokingState)
  : ParserRuleContext(parent, invokingState) {
}

tree::TerminalNode* KqlParser::LiteralContext::QUOTED_STRING() {
  return getToken(KqlParser::QUOTED_STRING, 0);
}

tree::TerminalNode* KqlParser::LiteralContext::UNQUOTED_LITERAL() {
  return getToken(KqlParser::UNQUOTED_LITERAL, 0);
}


size_t KqlParser::LiteralContext::getRuleIndex() const {
  return KqlParser::RuleLiteral;
}


std::any KqlParser::LiteralContext::accept(tree::ParseTreeVisitor *visitor) {
  if (auto parserVisitor = dynamic_cast<KqlVisitor*>(visitor))
    return parserVisitor->visitLiteral(this);
  else
    return visitor->visitChildren(this);
}

KqlParser::LiteralContext* KqlParser::literal() {
  LiteralContext *_localctx = _tracker.createInstance<LiteralContext>(_ctx, getState());
  enterRule(_localctx, 20, KqlParser::RuleLiteral);
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
    setState(100);
    _la = _input->LA(1);
    if (!(_la == KqlParser::QUOTED_STRING

    || _la == KqlParser::UNQUOTED_LITERAL)) {
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

bool KqlParser::sempred(RuleContext *context, size_t ruleIndex, size_t predicateIndex) {
  switch (ruleIndex) {
    case 1: return querySempred(antlrcpp::downCast<QueryContext *>(context), predicateIndex);

  default:
    break;
  }
  return true;
}

bool KqlParser::querySempred(QueryContext *_localctx, size_t predicateIndex) {
  switch (predicateIndex) {
    case 0: return precpred(_ctx, 2);

  default:
    break;
  }
  return true;
}

void KqlParser::initialize() {
#if ANTLR4_USE_THREAD_LOCAL_CACHE
  kqlParserInitialize();
#else
  ::antlr4::internal::call_once(kqlParserOnceFlag, kqlParserInitialize);
#endif
}
