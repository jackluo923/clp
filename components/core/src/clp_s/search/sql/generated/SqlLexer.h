
// Generated from Sql.g4 by ANTLR 4.13.2

#pragma once


#include "antlr4-runtime.h"


namespace clp_s::search::sql::generated {


class  SqlLexer : public antlr4::Lexer {
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

  explicit SqlLexer(antlr4::CharStream *input);

  ~SqlLexer() override;


  std::string getGrammarFileName() const override;

  const std::vector<std::string>& getRuleNames() const override;

  const std::vector<std::string>& getChannelNames() const override;

  const std::vector<std::string>& getModeNames() const override;

  const antlr4::dfa::Vocabulary& getVocabulary() const override;

  antlr4::atn::SerializedATNView getSerializedATN() const override;

  const antlr4::atn::ATN& getATN() const override;

  // By default the static state used to implement the lexer is lazily initialized during the first
  // call to the constructor. You can call this function if you wish to initialize the static state
  // ahead of time.
  static void initialize();

private:

  // Individual action functions triggered by action() above.

  // Individual semantic predicate functions triggered by sempred() above.

};

}  // namespace clp_s::search::sql::generated
