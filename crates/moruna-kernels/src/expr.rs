//! The expression language of `moruna.std.filter` (MH 4.9).
//!
//! Small on purpose: comparisons of a column with a literal or with another column, `is_null`
//! and `is_not_null`, and `and`, `or`, `not` and parentheses. Anything richer belongs in a
//! kernel an author writes.
//!
//! ```text
//! expr    := or
//! or      := and ("or" and)*
//! and     := unary ("and" unary)*
//! unary   := "not" unary | primary
//! primary := "(" expr ")" | ("is_null" | "is_not_null") "(" column ")" | operand op operand
//! operand := column | literal
//! op      := "==" | "!=" | "<" | "<=" | ">" | ">="
//! column  := [A-Za-z_][A-Za-z0-9_.]* | "`" any but "`" "`"
//! literal := integer | float | 'string' | "string" | true | false
//! ```

use std::sync::Arc;

use moruna_kernel::arrow::array::{
    Array, ArrayRef, BooleanArray, Datum, Float64Array, Int64Array, Scalar, StringArray,
};
use moruna_kernel::arrow::compute::kernels::{boolean, cast, cmp};
use moruna_kernel::arrow::datatypes::{DataType, Schema};
use moruna_kernel::arrow::record_batch::RecordBatch;
use moruna_kernel::declare::{ColumnDecl, TypeDecl};
use moruna_kernel::{MorunaError, Result};

/// A literal of the expression language.
#[derive(Clone, Debug, PartialEq)]
pub enum Literal {
    /// An integer.
    Int(i64),
    /// A float.
    Float(f64),
    /// A string.
    Str(String),
    /// A boolean.
    Bool(bool),
}

impl Literal {
    /// The literal as a one-element array of its own type.
    pub(crate) fn array(&self) -> ArrayRef {
        match self {
            Literal::Int(v) => Arc::new(Int64Array::from(vec![*v])),
            Literal::Float(v) => Arc::new(Float64Array::from(vec![*v])),
            Literal::Str(v) => Arc::new(StringArray::from(vec![v.as_str()])),
            Literal::Bool(v) => Arc::new(BooleanArray::from(vec![*v])),
        }
    }

    /// The type `moruna check` generates for a column compared with this literal.
    pub(crate) fn data_type(&self) -> DataType {
        match self {
            Literal::Int(_) => DataType::Int64,
            Literal::Float(_) => DataType::Float64,
            Literal::Str(_) => DataType::Utf8,
            Literal::Bool(_) => DataType::Boolean,
        }
    }

    /// The literal from a JSON value (`fill_null`'s values).
    pub(crate) fn from_json(value: &serde_json::Value) -> Option<Literal> {
        match value {
            serde_json::Value::Bool(b) => Some(Literal::Bool(*b)),
            serde_json::Value::String(s) => Some(Literal::Str(s.clone())),
            serde_json::Value::Number(n) => n
                .as_i64()
                .map(Literal::Int)
                .or_else(|| n.as_f64().map(Literal::Float)),
            _ => None,
        }
    }

    /// The literal as a scalar of `ty`, cast strictly, so `a > 'x'` on an integer column is an
    /// error at plan time and not a silent null.
    pub(crate) fn scalar_of(&self, ty: &DataType) -> Result<Scalar<ArrayRef>> {
        let textual = matches!(
            ty,
            DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View | DataType::Binary
        );
        if textual && !matches!(self, Literal::Str(_)) {
            return Err(MorunaError::Plan(format!(
                "the literal {self:?} is not a {ty}; quote it to compare text"
            )));
        }
        let options = cast::CastOptions {
            safe: false,
            ..Default::default()
        };
        let cast = cast::cast_with_options(&self.array(), ty, &options)
            .map_err(|e| MorunaError::Plan(format!("the literal {self:?} is not a {ty}: {e}")))?;
        Ok(Scalar::new(cast))
    }
}

/// A comparison operator.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Op {
    /// `==`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `>`
    Gt,
    /// `>=`
    Ge,
}

/// One side of a comparison.
#[derive(Clone, Debug, PartialEq)]
pub enum Operand {
    /// A column by name.
    Column(String),
    /// A literal.
    Literal(Literal),
}

/// A parsed predicate.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// `a op b`.
    Cmp(Operand, Op, Operand),
    /// `is_null(c)`.
    IsNull(String),
    /// `is_not_null(c)`.
    IsNotNull(String),
    /// `a and b`.
    And(Box<Expr>, Box<Expr>),
    /// `a or b`.
    Or(Box<Expr>, Box<Expr>),
    /// `not a`.
    Not(Box<Expr>),
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Ident(String),
    Quoted(String),
    Str(String),
    Num(String),
    Op(Op),
    LParen,
    RParen,
}

fn tokens(text: &str) -> Result<Vec<Token>> {
    let bad = |msg: String| MorunaError::Plan(format!("moruna.std.filter: {msg} in `{text}`"));
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            out.push(Token::LParen);
            i += 1;
        } else if c == ')' {
            out.push(Token::RParen);
            i += 1;
        } else if c == '\'' || c == '"' || c == '`' {
            let close = chars[i + 1..]
                .iter()
                .position(|&d| d == c)
                .ok_or_else(|| bad(format!("unclosed {c}")))?;
            let body: String = chars[i + 1..i + 1 + close].iter().collect();
            out.push(if c == '`' {
                Token::Quoted(body)
            } else {
                Token::Str(body)
            });
            i += close + 2;
        } else if "=!<>".contains(c) {
            let next = chars.get(i + 1).copied();
            let (op, len) = match (c, next) {
                ('=', Some('=')) => (Op::Eq, 2),
                ('!', Some('=')) => (Op::Ne, 2),
                ('<', Some('=')) => (Op::Le, 2),
                ('>', Some('=')) => (Op::Ge, 2),
                ('<', _) => (Op::Lt, 1),
                ('>', _) => (Op::Gt, 1),
                _ => return Err(bad(format!("unknown operator at `{c}`"))),
            };
            out.push(Token::Op(op));
            i += len;
        } else if c.is_ascii_digit()
            || (c == '-' && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            let start = i;
            i += 1;
            while i < chars.len() && (chars[i].is_ascii_digit() || ".eE+-".contains(chars[i])) {
                if (chars[i] == '+' || chars[i] == '-') && !matches!(chars[i - 1], 'e' | 'E') {
                    break;
                }
                i += 1;
            }
            out.push(Token::Num(chars[start..i].iter().collect()));
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '.')
            {
                i += 1;
            }
            out.push(Token::Ident(chars[start..i].iter().collect()));
        } else {
            return Err(bad(format!("unexpected `{c}`")));
        }
    }
    Ok(out)
}

struct Parser<'a> {
    text: &'a str,
    tokens: Vec<Token>,
    at: usize,
}

impl Parser<'_> {
    fn bad(&self, msg: &str) -> MorunaError {
        MorunaError::Plan(format!("moruna.std.filter: {msg} in `{}`", self.text))
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn keyword(&mut self, word: &str) -> bool {
        if matches!(self.peek(), Some(Token::Ident(w)) if w == word) {
            self.at += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: Token, what: &str) -> Result<()> {
        if self.peek() == Some(&token) {
            self.at += 1;
            Ok(())
        } else {
            Err(self.bad(&format!("expected {what}")))
        }
    }

    fn or(&mut self) -> Result<Expr> {
        let mut left = self.and()?;
        while self.keyword("or") {
            left = Expr::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expr> {
        let mut left = self.unary()?;
        while self.keyword("and") {
            left = Expr::And(Box::new(left), Box::new(self.unary()?));
        }
        Ok(left)
    }

    fn unary(&mut self) -> Result<Expr> {
        if self.keyword("not") {
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.primary()
    }

    fn column_arg(&mut self) -> Result<String> {
        self.expect(Token::LParen, "(")?;
        let name = match self.peek().cloned() {
            Some(Token::Ident(n)) | Some(Token::Quoted(n)) => n,
            _ => return Err(self.bad("expected a column name")),
        };
        self.at += 1;
        self.expect(Token::RParen, ")")?;
        Ok(name)
    }

    fn primary(&mut self) -> Result<Expr> {
        if self.peek() == Some(&Token::LParen) {
            self.at += 1;
            let inner = self.or()?;
            self.expect(Token::RParen, ")")?;
            return Ok(inner);
        }
        if self.keyword("is_null") {
            return Ok(Expr::IsNull(self.column_arg()?));
        }
        if self.keyword("is_not_null") {
            return Ok(Expr::IsNotNull(self.column_arg()?));
        }
        let left = self.operand()?;
        let op = match self.peek() {
            Some(Token::Op(op)) => *op,
            _ => return Err(self.bad("expected a comparison")),
        };
        self.at += 1;
        let right = self.operand()?;
        if matches!((&left, &right), (Operand::Literal(_), Operand::Literal(_))) {
            return Err(self.bad("a comparison needs at least one column"));
        }
        Ok(Expr::Cmp(left, op, right))
    }

    fn operand(&mut self) -> Result<Operand> {
        let token = self
            .peek()
            .cloned()
            .ok_or_else(|| self.bad("unexpected end"))?;
        self.at += 1;
        Ok(match token {
            Token::Ident(w) if w == "true" => Operand::Literal(Literal::Bool(true)),
            Token::Ident(w) if w == "false" => Operand::Literal(Literal::Bool(false)),
            Token::Ident(w) if ["and", "or", "not"].contains(&w.as_str()) => {
                return Err(self.bad(&format!("unexpected `{w}`")));
            }
            Token::Ident(w) | Token::Quoted(w) => Operand::Column(w),
            Token::Str(s) => Operand::Literal(Literal::Str(s)),
            Token::Num(n) => match n.parse::<i64>() {
                Ok(v) => Operand::Literal(Literal::Int(v)),
                Err(_) => Operand::Literal(Literal::Float(
                    n.parse::<f64>()
                        .map_err(|_| self.bad(&format!("bad number `{n}`")))?,
                )),
            },
            _ => return Err(self.bad("expected a column or a literal")),
        })
    }
}

impl Expr {
    /// Parse `text`; a `Plan` error names what was wrong and quotes the text.
    pub fn parse(text: &str) -> Result<Expr> {
        let mut parser = Parser {
            text,
            tokens: tokens(text)?,
            at: 0,
        };
        let expr = parser.or()?;
        if parser.at != parser.tokens.len() {
            return Err(parser.bad("unexpected text after the expression"));
        }
        Ok(expr)
    }

    /// The columns the predicate reads, first mention first, with the type `moruna check`
    /// generates for each: a literal's type for a column compared with one, `any` otherwise.
    pub fn columns(&self) -> Vec<ColumnDecl> {
        let mut out: Vec<ColumnDecl> = Vec::new();
        self.walk(&mut |name, ty| {
            if let Some(existing) = out.iter_mut().find(|c| c.name == name) {
                if existing.ty == TypeDecl::Any {
                    existing.ty = ty;
                }
            } else {
                out.push(ColumnDecl {
                    name: name.to_string(),
                    ty,
                    nullable: true,
                });
            }
        });
        out
    }

    fn walk(&self, f: &mut dyn FnMut(&str, TypeDecl)) {
        match self {
            Expr::Cmp(l, _, r) => {
                let ty = |other: &Operand| match other {
                    Operand::Literal(lit) => TypeDecl::Exact(lit.data_type()),
                    Operand::Column(_) => TypeDecl::Any,
                };
                if let Operand::Column(c) = l {
                    f(c, ty(r));
                }
                if let Operand::Column(c) = r {
                    f(c, ty(l));
                }
            }
            Expr::IsNull(c) | Expr::IsNotNull(c) => f(c, TypeDecl::Any),
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.walk(f);
                b.walk(f);
            }
            Expr::Not(a) => a.walk(f),
        }
    }

    /// Plan-time check against a schema: every column exists and every literal converts to its
    /// column's type.
    pub fn validate(&self, schema: &Schema) -> Result<()> {
        match self {
            Expr::Cmp(l, _, r) => {
                let ty = |o: &Operand| -> Result<Option<DataType>> {
                    match o {
                        Operand::Column(c) => column_type(schema, c).map(Some),
                        Operand::Literal(_) => Ok(None),
                    }
                };
                let (lt, rt) = (ty(l)?, ty(r)?);
                match (l, r, lt, rt) {
                    (_, Operand::Literal(lit), Some(t), _)
                    | (Operand::Literal(lit), _, _, Some(t)) => lit.scalar_of(&t).map(|_| ()),
                    _ => Ok(()),
                }
            }
            Expr::IsNull(c) | Expr::IsNotNull(c) => column_type(schema, c).map(|_| ()),
            Expr::And(a, b) | Expr::Or(a, b) => {
                a.validate(schema)?;
                b.validate(schema)
            }
            Expr::Not(a) => a.validate(schema),
        }
    }

    /// Evaluate over `batch`; null where SQL's three-valued logic says so.
    pub fn eval(&self, batch: &RecordBatch) -> Result<BooleanArray> {
        let err = |e: moruna_kernel::arrow::error::ArrowError| {
            MorunaError::Plan(format!("moruna.std.filter: {e}"))
        };
        Ok(match self {
            Expr::Cmp(l, op, r) => {
                let (left, right) = (datum(batch, l, r)?, datum(batch, r, l)?);
                let (left, right): (&dyn Datum, &dyn Datum) = (left.as_ref(), right.as_ref());
                match op {
                    Op::Eq => cmp::eq(left, right),
                    Op::Ne => cmp::neq(left, right),
                    Op::Lt => cmp::lt(left, right),
                    Op::Le => cmp::lt_eq(left, right),
                    Op::Gt => cmp::gt(left, right),
                    Op::Ge => cmp::gt_eq(left, right),
                }
                .map_err(err)?
            }
            Expr::IsNull(c) => boolean::is_null(column(batch, c)?.as_ref()).map_err(err)?,
            Expr::IsNotNull(c) => boolean::is_not_null(column(batch, c)?.as_ref()).map_err(err)?,
            Expr::And(a, b) => {
                boolean::and_kleene(&a.eval(batch)?, &b.eval(batch)?).map_err(err)?
            }
            Expr::Or(a, b) => boolean::or_kleene(&a.eval(batch)?, &b.eval(batch)?).map_err(err)?,
            Expr::Not(a) => boolean::not(&a.eval(batch)?).map_err(err)?,
        })
    }
}

fn column_type(schema: &Schema, name: &str) -> Result<DataType> {
    schema
        .field_with_name(name)
        .map(|f| f.data_type().clone())
        .map_err(|_| MorunaError::Plan(format!("moruna.std.filter: no column `{name}`")))
}

fn column(batch: &RecordBatch, name: &str) -> Result<ArrayRef> {
    batch
        .column_by_name(name)
        .cloned()
        .ok_or_else(|| MorunaError::Plan(format!("moruna.std.filter: no column `{name}`")))
}

/// One side of a comparison as a datum: a column as it is, a literal as a scalar of the other
/// side's column type.
fn datum(batch: &RecordBatch, this: &Operand, other: &Operand) -> Result<Box<dyn Datum>> {
    match this {
        Operand::Column(c) => Ok(Box::new(column(batch, c)?)),
        Operand::Literal(lit) => {
            let Operand::Column(c) = other else {
                return Err(MorunaError::Plan(
                    "moruna.std.filter: a comparison needs at least one column".into(),
                ));
            };
            let ty = column(batch, c)?.data_type().clone();
            Ok(Box::new(lit.scalar_of(&ty)?))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moruna_kernel::arrow::datatypes::Field;

    fn batch() -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![
                Field::new("a", DataType::Int64, true),
                Field::new("b", DataType::Utf8, true),
                Field::new("my col", DataType::Float64, true),
            ])),
            vec![
                Arc::new(Int64Array::from(vec![Some(1), Some(5), None, Some(9)])),
                Arc::new(StringArray::from(vec![
                    Some("x"),
                    Some("y"),
                    Some("x"),
                    None,
                ])),
                Arc::new(Float64Array::from(vec![
                    Some(0.5),
                    Some(1.5),
                    Some(2.5),
                    None,
                ])),
            ],
        )
        .expect("batch")
    }

    fn eval(text: &str) -> Vec<Option<bool>> {
        Expr::parse(text)
            .expect("parse")
            .eval(&batch())
            .expect("eval")
            .iter()
            .collect()
    }

    #[test]
    fn comparisons_and_logic_follow_three_valued_logic() {
        assert_eq!(
            eval("a > 3"),
            vec![Some(false), Some(true), None, Some(true)]
        );
        assert_eq!(
            eval("3 < a"),
            vec![Some(false), Some(true), None, Some(true)]
        );
        assert_eq!(
            eval("a >= 5 and b == 'y'"),
            vec![Some(false), Some(true), Some(false), None]
        );
        assert_eq!(
            eval("a == 1 or b != \"x\""),
            vec![Some(true), Some(true), None, None]
        );
        assert_eq!(
            eval("not (a <= 1)"),
            vec![Some(false), Some(true), None, Some(true)]
        );
        assert_eq!(
            eval("is_null(a)"),
            vec![Some(false), Some(false), Some(true), Some(false)]
        );
        assert_eq!(
            eval("is_not_null(b)"),
            vec![Some(true), Some(true), Some(true), Some(false)]
        );
        assert_eq!(
            eval("`my col` < 2.0"),
            vec![Some(true), Some(true), Some(false), None]
        );
        assert_eq!(
            eval("`my col` < 1e1"),
            vec![Some(true), Some(true), Some(true), None]
        );
        assert_eq!(
            eval("a < -1"),
            vec![Some(false), Some(false), None, Some(false)]
        );
    }

    #[test]
    fn columns_carry_the_type_of_what_they_are_compared_with() {
        let e =
            Expr::parse("a > 3 and b == 'x' and is_null(c) and d < e and f == true and g == 1.5")
                .expect("parse");
        let got: Vec<(String, String)> = e
            .columns()
            .into_iter()
            .map(|c| (c.name, c.ty.spelling()))
            .collect();
        let want = [
            ("a", "int64"),
            ("b", "string"),
            ("c", "any"),
            ("d", "any"),
            ("e", "any"),
            ("f", "bool"),
            ("g", "double"),
        ];
        let want: Vec<(String, String)> = want
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect();
        assert_eq!(got, want);
        let again = Expr::parse("is_null(a) or a > 1").expect("parse").columns();
        assert_eq!(again[0].ty.spelling(), "int64");
    }

    #[test]
    fn malformed_expressions_are_plan_errors() {
        for bad in [
            "a >",
            "a > 'x",
            "(a > 1",
            "a > 1 b",
            "1 > 2",
            "a ~ 1",
            "a = 1",
            "is_null(1)",
            "and > 1",
            "a > 1e+e",
            "",
            "a > )",
        ] {
            assert!(Expr::parse(bad).is_err(), "{bad} should not parse");
        }
    }

    #[test]
    fn validation_names_the_missing_column_and_the_bad_literal() {
        let schema = batch().schema();
        assert!(Expr::parse("zz > 1").expect("p").validate(&schema).is_err());
        assert!(
            Expr::parse("a > 'x'")
                .expect("p")
                .validate(&schema)
                .is_err()
        );
        assert!(
            Expr::parse("is_null(zz)")
                .expect("p")
                .validate(&schema)
                .is_err()
        );
        assert!(
            Expr::parse("not a > 1 or a < b")
                .expect("p")
                .validate(&schema)
                .is_ok()
        );
        assert!(
            Expr::parse("'x' == b")
                .expect("p")
                .validate(&schema)
                .is_ok()
        );
    }

    #[test]
    fn literals_from_json() {
        use serde_json::json;
        assert_eq!(Literal::from_json(&json!(1)), Some(Literal::Int(1)));
        assert_eq!(Literal::from_json(&json!(1.5)), Some(Literal::Float(1.5)));
        assert_eq!(
            Literal::from_json(&json!("s")),
            Some(Literal::Str("s".into()))
        );
        assert_eq!(
            Literal::from_json(&json!(false)),
            Some(Literal::Bool(false))
        );
        assert_eq!(Literal::from_json(&json!(null)), None);
    }
}
