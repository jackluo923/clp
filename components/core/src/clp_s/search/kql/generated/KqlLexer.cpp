
// Generated from Kql.g4 by ANTLR 4.13.2


#include "KqlLexer.h"


using namespace antlr4;

using namespace clp_s::search::kql::generated;


using namespace antlr4;

namespace {

struct KqlLexerStaticData final {
  KqlLexerStaticData(std::vector<std::string> ruleNames,
                          std::vector<std::string> channelNames,
                          std::vector<std::string> modeNames,
                          std::vector<std::string> literalNames,
                          std::vector<std::string> symbolicNames)
      : ruleNames(std::move(ruleNames)), channelNames(std::move(channelNames)),
        modeNames(std::move(modeNames)), literalNames(std::move(literalNames)),
        symbolicNames(std::move(symbolicNames)),
        vocabulary(this->literalNames, this->symbolicNames) {}

  KqlLexerStaticData(const KqlLexerStaticData&) = delete;
  KqlLexerStaticData(KqlLexerStaticData&&) = delete;
  KqlLexerStaticData& operator=(const KqlLexerStaticData&) = delete;
  KqlLexerStaticData& operator=(KqlLexerStaticData&&) = delete;

  std::vector<antlr4::dfa::DFA> decisionToDFA;
  antlr4::atn::PredictionContextCache sharedContextCache;
  const std::vector<std::string> ruleNames;
  const std::vector<std::string> channelNames;
  const std::vector<std::string> modeNames;
  const std::vector<std::string> literalNames;
  const std::vector<std::string> symbolicNames;
  const antlr4::dfa::Vocabulary vocabulary;
  antlr4::atn::SerializedATNView serializedATN;
  std::unique_ptr<antlr4::atn::ATN> atn;
};

::antlr4::internal::OnceFlag kqllexerLexerOnceFlag;
#if ANTLR4_USE_THREAD_LOCAL_CACHE
static thread_local
#endif
std::unique_ptr<KqlLexerStaticData> kqllexerLexerStaticData = nullptr;

void kqllexerLexerInitialize() {
#if ANTLR4_USE_THREAD_LOCAL_CACHE
  if (kqllexerLexerStaticData != nullptr) {
    return;
  }
#else
  assert(kqllexerLexerStaticData == nullptr);
#endif
  auto staticData = std::make_unique<KqlLexerStaticData>(
    std::vector<std::string>{
      "T__0", "T__1", "T__2", "T__3", "T__4", "T__5", "T__6", "T__7", "AND", 
      "OR", "NOT", "QUOTED_STRING", "UNQUOTED_LITERAL", "QUOTED_CHARACTER", 
      "UNQUOTED_CHARACTER", "WILDCARD", "RANGE_OPERATOR", "ESCAPED_SPECIAL_CHARACTER", 
      "ESCAPED_SPACE", "SPECIAL_CHARACTER", "UNICODE", "HEXDIGIT", "SPACE"
    },
    std::vector<std::string>{
      "DEFAULT_TOKEN_CHANNEL", "HIDDEN"
    },
    std::vector<std::string>{
      "DEFAULT_MODE"
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
  	4,0,15,154,6,-1,2,0,7,0,2,1,7,1,2,2,7,2,2,3,7,3,2,4,7,4,2,5,7,5,2,6,7,
  	6,2,7,7,7,2,8,7,8,2,9,7,9,2,10,7,10,2,11,7,11,2,12,7,12,2,13,7,13,2,14,
  	7,14,2,15,7,15,2,16,7,16,2,17,7,17,2,18,7,18,2,19,7,19,2,20,7,20,2,21,
  	7,21,2,22,7,22,1,0,1,0,1,1,1,1,1,2,1,2,1,3,1,3,1,4,1,4,1,5,1,5,1,5,1,
  	5,1,5,1,5,1,5,1,5,1,5,1,5,1,6,1,6,1,7,1,7,1,7,1,7,1,7,1,7,1,7,1,7,1,7,
  	1,7,1,7,1,8,1,8,1,8,1,8,1,9,1,9,1,9,1,10,1,10,1,10,1,10,1,11,1,11,5,11,
  	94,8,11,10,11,12,11,97,9,11,1,11,1,11,1,12,4,12,102,8,12,11,12,12,12,
  	103,1,13,1,13,1,13,1,13,3,13,110,8,13,1,14,1,14,1,14,1,14,1,14,3,14,117,
  	8,14,1,15,1,15,1,16,1,16,1,16,1,16,1,16,3,16,126,8,16,1,17,1,17,1,17,
  	1,18,1,18,1,18,1,18,1,18,1,18,3,18,137,8,18,1,19,1,19,1,20,1,20,1,20,
  	1,20,1,20,1,20,1,20,1,20,1,21,1,21,1,22,1,22,1,22,1,22,0,0,23,1,1,3,2,
  	5,3,7,4,9,5,11,6,13,7,15,8,17,9,19,10,21,11,23,12,25,13,27,0,29,0,31,
  	0,33,14,35,0,37,0,39,0,41,0,43,0,45,15,1,0,13,2,0,65,65,97,97,2,0,78,
  	78,110,110,2,0,68,68,100,100,2,0,79,79,111,111,2,0,82,82,114,114,2,0,
  	84,84,116,116,1,0,34,34,11,0,9,10,13,13,32,32,34,34,40,41,58,58,60,60,
  	62,62,92,92,123,123,125,125,2,0,42,42,63,63,2,0,60,60,62,62,9,0,33,36,
  	40,42,46,46,58,58,60,60,62,64,92,92,123,123,125,125,3,0,48,57,65,70,97,
  	102,3,0,9,10,13,13,32,32,157,0,1,1,0,0,0,0,3,1,0,0,0,0,5,1,0,0,0,0,7,
  	1,0,0,0,0,9,1,0,0,0,0,11,1,0,0,0,0,13,1,0,0,0,0,15,1,0,0,0,0,17,1,0,0,
  	0,0,19,1,0,0,0,0,21,1,0,0,0,0,23,1,0,0,0,0,25,1,0,0,0,0,33,1,0,0,0,0,
  	45,1,0,0,0,1,47,1,0,0,0,3,49,1,0,0,0,5,51,1,0,0,0,7,53,1,0,0,0,9,55,1,
  	0,0,0,11,57,1,0,0,0,13,67,1,0,0,0,15,69,1,0,0,0,17,80,1,0,0,0,19,84,1,
  	0,0,0,21,87,1,0,0,0,23,91,1,0,0,0,25,101,1,0,0,0,27,109,1,0,0,0,29,116,
  	1,0,0,0,31,118,1,0,0,0,33,125,1,0,0,0,35,127,1,0,0,0,37,136,1,0,0,0,39,
  	138,1,0,0,0,41,140,1,0,0,0,43,148,1,0,0,0,45,150,1,0,0,0,47,48,5,58,0,
  	0,48,2,1,0,0,0,49,50,5,123,0,0,50,4,1,0,0,0,51,52,5,125,0,0,52,6,1,0,
  	0,0,53,54,5,40,0,0,54,8,1,0,0,0,55,56,5,41,0,0,56,10,1,0,0,0,57,58,5,
  	115,0,0,58,59,5,101,0,0,59,60,5,109,0,0,60,61,5,97,0,0,61,62,5,110,0,
  	0,62,63,5,116,0,0,63,64,5,105,0,0,64,65,5,99,0,0,65,66,5,40,0,0,66,12,
  	1,0,0,0,67,68,5,44,0,0,68,14,1,0,0,0,69,70,5,116,0,0,70,71,5,105,0,0,
  	71,72,5,109,0,0,72,73,5,101,0,0,73,74,5,115,0,0,74,75,5,116,0,0,75,76,
  	5,97,0,0,76,77,5,109,0,0,77,78,5,112,0,0,78,79,5,40,0,0,79,16,1,0,0,0,
  	80,81,7,0,0,0,81,82,7,1,0,0,82,83,7,2,0,0,83,18,1,0,0,0,84,85,7,3,0,0,
  	85,86,7,4,0,0,86,20,1,0,0,0,87,88,7,1,0,0,88,89,7,3,0,0,89,90,7,5,0,0,
  	90,22,1,0,0,0,91,95,5,34,0,0,92,94,3,27,13,0,93,92,1,0,0,0,94,97,1,0,
  	0,0,95,93,1,0,0,0,95,96,1,0,0,0,96,98,1,0,0,0,97,95,1,0,0,0,98,99,5,34,
  	0,0,99,24,1,0,0,0,100,102,3,29,14,0,101,100,1,0,0,0,102,103,1,0,0,0,103,
  	101,1,0,0,0,103,104,1,0,0,0,104,26,1,0,0,0,105,110,3,37,18,0,106,107,
  	5,92,0,0,107,110,5,34,0,0,108,110,8,6,0,0,109,105,1,0,0,0,109,106,1,0,
  	0,0,109,108,1,0,0,0,110,28,1,0,0,0,111,117,3,37,18,0,112,117,3,35,17,
  	0,113,117,3,31,15,0,114,117,3,41,20,0,115,117,8,7,0,0,116,111,1,0,0,0,
  	116,112,1,0,0,0,116,113,1,0,0,0,116,114,1,0,0,0,116,115,1,0,0,0,117,30,
  	1,0,0,0,118,119,7,8,0,0,119,32,1,0,0,0,120,121,5,60,0,0,121,126,5,61,
  	0,0,122,123,5,62,0,0,123,126,5,61,0,0,124,126,7,9,0,0,125,120,1,0,0,0,
  	125,122,1,0,0,0,125,124,1,0,0,0,126,34,1,0,0,0,127,128,5,92,0,0,128,129,
  	3,39,19,0,129,36,1,0,0,0,130,131,5,92,0,0,131,137,5,116,0,0,132,133,5,
  	92,0,0,133,137,5,114,0,0,134,135,5,92,0,0,135,137,5,110,0,0,136,130,1,
  	0,0,0,136,132,1,0,0,0,136,134,1,0,0,0,137,38,1,0,0,0,138,139,7,10,0,0,
  	139,40,1,0,0,0,140,141,5,92,0,0,141,142,5,117,0,0,142,143,1,0,0,0,143,
  	144,3,43,21,0,144,145,3,43,21,0,145,146,3,43,21,0,146,147,3,43,21,0,147,
  	42,1,0,0,0,148,149,7,11,0,0,149,44,1,0,0,0,150,151,7,12,0,0,151,152,1,
  	0,0,0,152,153,6,22,0,0,153,46,1,0,0,0,7,0,95,103,109,116,125,136,1,6,
  	0,0
  };
  staticData->serializedATN = antlr4::atn::SerializedATNView(serializedATNSegment, sizeof(serializedATNSegment) / sizeof(serializedATNSegment[0]));

  antlr4::atn::ATNDeserializer deserializer;
  staticData->atn = deserializer.deserialize(staticData->serializedATN);

  const size_t count = staticData->atn->getNumberOfDecisions();
  staticData->decisionToDFA.reserve(count);
  for (size_t i = 0; i < count; i++) { 
    staticData->decisionToDFA.emplace_back(staticData->atn->getDecisionState(i), i);
  }
  kqllexerLexerStaticData = std::move(staticData);
}

}

KqlLexer::KqlLexer(CharStream *input) : Lexer(input) {
  KqlLexer::initialize();
  _interpreter = new atn::LexerATNSimulator(this, *kqllexerLexerStaticData->atn, kqllexerLexerStaticData->decisionToDFA, kqllexerLexerStaticData->sharedContextCache);
}

KqlLexer::~KqlLexer() {
  delete _interpreter;
}

std::string KqlLexer::getGrammarFileName() const {
  return "Kql.g4";
}

const std::vector<std::string>& KqlLexer::getRuleNames() const {
  return kqllexerLexerStaticData->ruleNames;
}

const std::vector<std::string>& KqlLexer::getChannelNames() const {
  return kqllexerLexerStaticData->channelNames;
}

const std::vector<std::string>& KqlLexer::getModeNames() const {
  return kqllexerLexerStaticData->modeNames;
}

const dfa::Vocabulary& KqlLexer::getVocabulary() const {
  return kqllexerLexerStaticData->vocabulary;
}

antlr4::atn::SerializedATNView KqlLexer::getSerializedATN() const {
  return kqllexerLexerStaticData->serializedATN;
}

const atn::ATN& KqlLexer::getATN() const {
  return *kqllexerLexerStaticData->atn;
}




void KqlLexer::initialize() {
#if ANTLR4_USE_THREAD_LOCAL_CACHE
  kqllexerLexerInitialize();
#else
  ::antlr4::internal::call_once(kqllexerLexerOnceFlag, kqllexerLexerInitialize);
#endif
}
