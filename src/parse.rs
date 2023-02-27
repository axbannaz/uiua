use std::{error::Error, fmt, path::Path};

use crate::{
    ast::*,
    lex::{Simple::*, *},
};

#[derive(Debug)]
pub enum ParseError {
    Lex(LexError),
    Expected(Vec<Expectation>, Option<Box<Sp<Token>>>),
}

#[derive(Debug)]
pub enum Expectation {
    Ident,
    Block,
    Type,
    Expr,
    Pattern,
    Eof,
    Simple(Simple),
}

impl fmt::Display for Expectation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Expectation::Ident => write!(f, "identifier"),
            Expectation::Block => write!(f, "block"),
            Expectation::Type => write!(f, "type"),
            Expectation::Expr => write!(f, "expression"),
            Expectation::Pattern => write!(f, "pattern"),
            Expectation::Eof => write!(f, "end of file"),
            Expectation::Simple(s) => write!(f, "{s}"),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Lex(e) => write!(f, "{e}"),
            ParseError::Expected(exps, found) => {
                write!(f, "expected ")?;
                for (i, exp) in exps.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{exp}")?;
                }
                if let Some(found) = found {
                    write!(f, ", found {found:?}")?;
                }
                Ok(())
            }
        }
    }
}

impl Error for ParseError {}

pub type ParseResult<T = ()> = Result<T, Sp<ParseError>>;

pub fn parse(input: &str, path: &Path) -> Result<Vec<Item>, Vec<Sp<ParseError>>> {
    let tokens = lex(input, path).map_err(|e| vec![e.map(ParseError::Lex)])?;
    let mut items = Vec::new();
    let mut parser = Parser {
        tokens,
        index: 0,
        errors: Vec::new(),
    };
    while let Some(item) = parser.try_item().map_err(|e| vec![e])? {
        items.push(item);
    }
    parser.next_token_map::<()>(|_| None);
    if parser.index != parser.tokens.len() {
        parser.errors.push(parser.expected([Expectation::Eof]));
    }
    if parser.errors.is_empty() {
        Ok(items)
    } else {
        Err(parser.errors)
    }
}

struct Parser {
    tokens: Vec<Sp<crate::lex::Token>>,
    index: usize,
    errors: Vec<Sp<ParseError>>,
}

impl Parser {
    fn next_token_map<'a, T: 'a>(
        &'a mut self,
        f: impl FnOnce(&'a Token) -> Option<T>,
    ) -> Option<Sp<T>> {
        while let Some(token) = self.tokens.get(self.index) {
            if matches!(token.value, Token::Comment(_)) {
                self.index += 1;
            } else if let Some(value) = f(&token.value) {
                self.index += 1;
                return Some(token.span.clone().sp(value));
            } else {
                return None;
            }
        }
        None
    }
    fn try_exact(&mut self, token: impl Into<Token>) -> Option<Span> {
        let token = token.into();
        self.next_token_map(|t| (t == &token).then_some(()))
            .map(|t| t.span)
    }
    fn last_span(&self) -> Span {
        if let Some(token) = self.tokens.get(self.index) {
            token.span.clone()
        } else {
            let mut span = self.tokens.last().unwrap().span.clone();
            span.start = span.end;
            if self.tokens.len() > span.end.pos {
                span.end.pos += 1;
                span.end.col += 1;
            }
            span
        }
    }
    fn expect(&mut self, simple: Simple) -> ParseResult<Span> {
        self.try_exact(simple)
            .ok_or_else(|| self.expected([Expectation::Simple(simple)]))
    }
    fn expected(&self, expectations: impl IntoIterator<Item = Expectation>) -> Sp<ParseError> {
        self.last_span().sp(ParseError::Expected(
            expectations.into_iter().collect(),
            self.tokens.get(self.index).cloned().map(Box::new),
        ))
    }
    fn try_item(&mut self) -> ParseResult<Option<Item>> {
        Ok(Some(if let Some(def) = self.try_function_def()? {
            Item::FunctionDef(def)
        } else if let Some(binding) = self.try_binding()? {
            Item::Binding(binding)
        } else if let Some(expr) = self.try_expr()? {
            self.expect(SemiColon)?;
            Item::Expr(expr)
        } else {
            return Ok(None);
        }))
    }
    fn doc_comment(&mut self) -> Option<Sp<String>> {
        let mut doc: Option<Sp<String>> = None;
        while let Some(line) = self.next_token_map(|token| {
            if let Token::DocComment(line) = token {
                Some(line)
            } else {
                None
            }
        }) {
            if let Some(doc) = &mut doc {
                doc.value.push_str(line.value);
                doc.span.end = line.span.end;
            } else {
                doc = Some(line.cloned());
            }
        }
        doc
    }
    fn try_function_def(&mut self) -> ParseResult<Option<FunctionDef>> {
        // Documentation comments
        let doc = self.doc_comment();
        // Keyword
        if self.try_exact(Keyword::Fn).is_none() {
            return Ok(None);
        };
        // Name
        let name = self.ident()?;
        // Parameters
        let params = self
            .surrounded_list(OpenParen, CloseParen, Self::try_param)?
            .value;
        // Body
        let body = self.block()?.value;
        Ok(Some(FunctionDef {
            doc,
            name,
            params,
            body,
        }))
    }
    fn try_binding(&mut self) -> ParseResult<Option<Binding>> {
        if self.try_exact(Keyword::Let).is_none() {
            return Ok(None);
        };
        let pattern = self.pattern()?;
        self.expect(Equals)?;
        let expr = self.expr()?;
        self.expect(SemiColon)?;
        Ok(Some(Binding { pattern, expr }))
    }
    fn try_pattern(&mut self) -> ParseResult<Option<Sp<Pattern>>> {
        Ok(Some(if let Some(ident) = self.try_ident() {
            ident.map(Pattern::Ident)
        } else if let Some(items) =
            self.try_surrounded_list(OpenParen, CloseParen, Self::try_pattern)?
        {
            items.map(Pattern::Tuple)
        } else {
            return Ok(None);
        }))
    }
    fn pattern(&mut self) -> ParseResult<Sp<Pattern>> {
        self.try_pattern()?
            .ok_or_else(|| self.expected([Expectation::Pattern]))
    }
    fn try_block(&mut self) -> ParseResult<Option<Sp<Block>>> {
        let Some(start) = self.try_exact(OpenCurly) else {
            return Ok(None);
        };
        let mut items = Vec::new();
        while let Some(expr) = self.try_item()? {
            items.push(expr);
        }
        let end = self.expect(CloseCurly)?;
        let span = start.merge(end);
        Ok(Some(span.sp(Block { items })))
    }
    fn block(&mut self) -> ParseResult<Sp<Block>> {
        self.try_block()?
            .ok_or_else(|| self.expected([Expectation::Block]))
    }
    fn try_param(&mut self) -> ParseResult<Option<Param>> {
        let Some(name) = self.try_ident() else {
            return Ok(None);
        };
        self.expect(Colon)?;
        let ty = self.ty()?;
        Ok(Some(Param { name, ty }))
    }
    fn try_ty(&mut self) -> ParseResult<Option<Sp<Type>>> {
        Ok(if let Some(ident) = self.try_ident() {
            // Named type
            Some(ident.map(Type::Ident))
        } else if self.try_exact(OpenBracket).is_some() {
            // Array
            let inner = self.ty()?;
            self.expect(CloseBracket)?;
            Some(inner.map(Box::new).map(Type::Array))
        } else if let Some(start) = self.try_exact(OpenParen) {
            // Tuple
            let mut tys = Vec::new();
            while let Some(ty) = self.try_ty()? {
                tys.push(ty);
                if self.try_exact(Comma).is_none() {
                    break;
                }
            }
            let end = self.expect(CloseParen)?;
            let span = start.merge(end);
            Some(span.sp(Type::Tuple(tys)))
        } else {
            None
        })
    }
    fn ty(&mut self) -> ParseResult<Sp<Type>> {
        self.try_ty()?
            .ok_or_else(|| self.expected([Expectation::Type]))
    }
    fn try_ident(&mut self) -> Option<Sp<String>> {
        self.next_token_map(|token| token.as_ident().map(Into::into))
    }
    fn ident(&mut self) -> ParseResult<Sp<String>> {
        self.try_ident()
            .ok_or_else(|| self.expected([Expectation::Ident]))
    }
    fn try_surrounded_list<T>(
        &mut self,
        open: Simple,
        close: Simple,
        item: impl Fn(&mut Self) -> ParseResult<Option<T>>,
    ) -> ParseResult<Option<Sp<Vec<T>>>> {
        let Some(start) = self.try_exact(open) else {
            return Ok(None);
        };
        let mut items = Vec::new();
        while let Some(item) = item(self)? {
            items.push(item);
            if self.try_exact(Comma).is_none() {
                break;
            }
        }
        let end = self.expect(close)?;
        let span = start.merge(end);
        Ok(Some(span.sp(items)))
    }
}

struct BinExprDef<'a> {
    ops: &'a [(Token, BinOp)],
    child: Option<&'a Self>,
}

static BIN_EXPR_OR: BinExprDef = BinExprDef {
    ops: &[(Token::Keyword(Keyword::Or), BinOp::Or)],
    child: Some(&BinExprDef {
        ops: &[(Token::Keyword(Keyword::And), BinOp::And)],
        child: Some(&BinExprDef {
            ops: &[
                (Token::Simple(Equal), BinOp::Eq),
                (Token::Simple(NotEqual), BinOp::Ne),
                (Token::Simple(Less), BinOp::Lt),
                (Token::Simple(LessEqual), BinOp::Le),
                (Token::Simple(Greater), BinOp::Gt),
                (Token::Simple(GreaterEqual), BinOp::Ge),
            ],
            child: Some(&BinExprDef {
                ops: &[
                    (Token::Simple(Plus), BinOp::Add),
                    (Token::Simple(Minus), BinOp::Sub),
                ],
                child: Some(&BinExprDef {
                    ops: &[
                        (Token::Simple(Star), BinOp::Mul),
                        (Token::Simple(Slash), BinOp::Div),
                    ],
                    child: None,
                }),
            }),
        }),
    }),
};

impl Parser {
    fn try_expr(&mut self) -> ParseResult<Option<Sp<Expr>>> {
        let Some(expr) = self.try_expr_impl()? else {
            return Ok(None);
        };
        Ok(Some(
            if let Some(op) = [
                (Equals, None),
                (PlusEquals, Some(BinOp::Add)),
                (MinusEquals, Some(BinOp::Sub)),
                (StarEquals, Some(BinOp::Mul)),
                (SlashEquals, Some(BinOp::Div)),
            ]
            .into_iter()
            .find_map(|(simple, op)| self.try_exact(simple).map(|span| span.sp(op)))
            {
                let rhs = self.expr()?;
                let lhs = expr;
                let start = lhs.span.clone();
                let end = rhs.span.clone();
                let span = start.merge(end);
                span.sp(Expr::Assign(Box::new(Assignment {
                    lvalue: lhs,
                    op: op.filter_map(|op| op),
                    expr: rhs,
                })))
            } else {
                expr
            },
        ))
    }
    fn expr(&mut self) -> ParseResult<Sp<Expr>> {
        self.expect_expr(Self::try_expr)
    }
    fn expect_expr(
        &mut self,
        f: impl FnOnce(&mut Self) -> ParseResult<Option<Sp<Expr>>>,
    ) -> ParseResult<Sp<Expr>> {
        f(self)?.ok_or_else(|| self.expected([Expectation::Expr]))
    }
    fn try_expr_impl(&mut self) -> ParseResult<Option<Sp<Expr>>> {
        self.try_bin_expr_def(&BIN_EXPR_OR)
    }
    fn try_bin_expr_def(&mut self, def: &BinExprDef) -> ParseResult<Option<Sp<Expr>>> {
        let leaf = |this: &mut Self| {
            if let Some(child) = def.child {
                this.try_bin_expr_def(child)
            } else {
                this.try_un_expr()
            }
        };
        let Some(mut lhs) = leaf(self)? else{
            return Ok(None);
        };
        'rhs: loop {
            for (token, op) in def.ops {
                if let Some(op_span) = self.try_exact(token.clone()) {
                    let rhs = self.expect_expr(leaf)?;
                    let start = lhs.span.clone();
                    let end = rhs.span.clone();
                    let span = start.merge(end);
                    let op = op_span.sp(*op);
                    lhs = span.sp(Expr::Bin(Box::new(BinExpr { lhs, op, rhs })));
                    continue 'rhs;
                }
            }
            return Ok(Some(lhs));
        }
    }
    fn try_un_expr(&mut self) -> ParseResult<Option<Sp<Expr>>> {
        if let Some(op) = [
            (Token::Simple(Minus), UnOp::Neg),
            (Token::Keyword(Keyword::Not), UnOp::Not),
        ]
        .into_iter()
        .find_map(|(token, op)| self.try_exact(token).map(|span| span.sp(op)))
        {
            let expr = self.expect_expr(Self::try_un_expr)?;
            let start = op.span.clone();
            let end = expr.span.clone();
            let span = start.merge(end);
            Ok(Some(span.sp(Expr::Un(Box::new(UnExpr { op, expr })))))
        } else {
            self.try_call_access()
        }
    }
    fn try_call_access(&mut self) -> ParseResult<Option<Sp<Expr>>> {
        let Some(mut expr) = self.try_term()? else {
            return Ok(None);
        };
        loop {
            if let Some(args) = self.try_surrounded_list(OpenParen, CloseParen, Self::try_expr)? {
                let start = expr.span.clone();
                let end = args.span.clone();
                let span = start.merge(end);
                expr = span.sp(Expr::Call(Box::new(Call {
                    expr,
                    args: args.value,
                })));
            } else if self.try_exact(Period).is_some() {
                let ident = self.ident()?;
                let start = expr.span.clone();
                let end = ident.span.clone();
                let span = start.merge(end);
                expr = span.sp(Expr::Access(Box::new(Access {
                    expr,
                    accessor: ident.map(Accessor::Field),
                })));
            } else {
                break;
            }
        }
        Ok(Some(expr))
    }
    fn surrounded_list<T>(
        &mut self,
        open: Simple,
        close: Simple,
        item: impl Fn(&mut Self) -> ParseResult<Option<T>>,
    ) -> ParseResult<Sp<Vec<T>>> {
        self.try_surrounded_list(open, close, item)?
            .ok_or_else(|| self.expected([Expectation::Simple(open)]))
    }
    fn try_term(&mut self) -> ParseResult<Option<Sp<Expr>>> {
        Ok(Some(if let Some(ident) = self.try_ident() {
            ident.map(Expr::Ident)
        } else if let Some(i) = self.next_token_map(Token::as_integer) {
            i.map(Into::into).map(Expr::Integer)
        } else if let Some(r) = self.next_token_map(Token::as_real) {
            r.map(Into::into).map(Expr::Real)
        } else if let Some(span) = self.try_exact(Keyword::True) {
            span.sp(Expr::Bool(true))
        } else if let Some(span) = self.try_exact(Keyword::False) {
            span.sp(Expr::Bool(false))
        } else if let Some(block) = self.try_block()? {
            block.map(Expr::Block)
        } else if let Some(items) =
            self.try_surrounded_list(OpenParen, CloseParen, Self::try_expr)?
        {
            items.map(Expr::Tuple)
        } else if let Some(items) =
            self.try_surrounded_list(OpenBracket, CloseBracket, Self::try_expr)?
        {
            items.map(Expr::Array)
        } else {
            return Ok(None);
        }))
    }
}
