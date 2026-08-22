//! Recursive-descent parser producing the template AST.

use omp_core::{Str, StrMut, sf};

use crate::{
	Value,
	error::{Error, Span, SyntaxErrorKind},
	lex::{self, SegKind, Segment, Tok, TokKind},
};

/// Identifiers with statement or operator meaning; rejected as variables.
const RESERVED: &[&str] = &[
	"if", "elif", "else", "endif", "for", "endfor", "in", "and", "or", "not", "set", "raw", "endraw",
];

/// One renderable AST node.
#[derive(Debug)]
pub enum Node {
	/// Literal output (zero-copy slice of the template source).
	Text(Str),
	/// `{{ expr }}`.
	Emit(Expr),
	/// `{% if %}` arms in order; the `else` arm has no condition.
	If(Vec<IfArm>),
	/// `{% for var in iter %}`.
	For { var: Str, iter: Expr, body: Vec<Self>, span: Span },
	/// `{% set name = value %}`.
	Set { name: Str, value: Expr },
	/// `{% name args %} … {% endname %}` block helper.
	Block { name: Str, name_span: Span, args: Vec<Expr>, body: Vec<Self> },
}

/// One `if`/`elif`/`else` arm.
#[derive(Debug)]
pub struct IfArm {
	/// `None` marks the `else` arm.
	pub cond: Option<Expr>,
	pub body: Vec<Node>,
}

/// An expression with the source span used for error reporting.
#[derive(Debug)]
pub enum Expr {
	Lit(Value),
	Var { name: Str, span: Span },
	Attr { base: Box<Self>, name: Str, optional: bool, span: Span },
	Index { base: Box<Self>, index: Box<Self>, optional: bool, span: Span },
	Not(Box<Self>),
	Neg(Box<Self>, Span),
	Bin { op: BinOp, lhs: Box<Self>, rhs: Box<Self>, span: Span },
	Ternary { cond: Box<Self>, then: Box<Self>, otherwise: Box<Self> },
	Filter { name: Str, name_span: Span, input: Box<Self>, args: Vec<Self> },
	Call { name: Str, name_span: Span, args: Vec<Self> },
}

/// Binary operators, lowest binding to highest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinOp {
	Or,
	And,
	Eq,
	Ne,
	Lt,
	Le,
	Gt,
	Ge,
	In,
	Concat,
	Add,
	Sub,
}

impl Expr {
	/// Reconstructs a dotted access path (`a.b[0]`) for undefined-key
	/// diagnostics; cold, error path only.
	pub(crate) fn path(&self) -> Str {
		let mut out = StrMut::with_capacity(24);
		self.write_path(&mut out);
		out.freeze()
	}

	fn write_path(&self, out: &mut StrMut) {
		match self {
			Self::Var { name, .. } => out.push_str(name),
			Self::Attr { base, name, optional, .. } => {
				base.write_path(out);
				out.push_str(if *optional { "?." } else { "." });
				out.push_str(name);
			},
			Self::Index { base, index, .. } => {
				base.write_path(out);
				out.push('[');
				match index.as_ref() {
					Self::Lit(Value::Str(key)) => {
						out.push('"');
						out.push_str(key);
						out.push('"');
					},
					Self::Lit(Value::Int(index)) => out.push_str(&sf!("{index}")),
					_ => out.push('…'),
				}
				out.push(']');
			},
			_ => out.push_str("<expr>"),
		}
	}
}

/// Parses `full` into a node list. Strips one trailing newline, segments the
/// source, applies whitespace control, and builds the statement tree.
pub fn parse(name: &Str, full: &Str) -> Result<Vec<Node>, Error> {
	let source = full.as_str();
	let effective = source.strip_suffix('\n').unwrap_or(source);
	let mut segments = lex::segment(name, effective)?;
	lex::apply_whitespace(effective, &mut segments);
	let mut builder = Builder { name, full, source: effective, segments: &segments, pos: 0 };
	let mut nodes = Vec::new();
	if let Some(end) = builder.build(&mut nodes)? {
		return Err(builder.syntax(end.span, SyntaxErrorKind::StrayEnd));
	}
	Ok(nodes)
}

/// A block terminator (`elif`/`else`/`endif`/`endfor`/`end<name>`) returned
/// from a nested body to the construct that owns it.
struct End {
	keyword: Str,
	span:    Span,
	/// Tokens after the keyword (an `elif` condition; empty otherwise).
	toks:    Vec<Tok>,
}

struct Builder<'b> {
	name:     &'b Str,
	full:     &'b Str,
	source:   &'b str,
	segments: &'b [Segment],
	pos:      usize,
}

impl<'b> Builder<'b> {
	fn syntax(&self, span: Span, kind: SyntaxErrorKind) -> Error {
		Error::syntax(self.name, self.source, span, kind)
	}

	fn slice(&self, span: Span) -> Str {
		let start = span.start as usize;
		self.full.slice(start..start + usize::from(span.len))
	}

	fn text(&self, span: Span) -> &'b str {
		let start = span.start as usize;
		&self.source[start..start + usize::from(span.len)]
	}

	/// Builds nodes until a terminator statement or the end of input.
	fn build(&mut self, out: &mut Vec<Node>) -> Result<Option<End>, Error> {
		while self.pos < self.segments.len() {
			let seg = self.segments[self.pos];
			self.pos += 1;
			match seg.kind {
				SegKind::Comment | SegKind::Marker => {},
				SegKind::Text => {
					if seg.end > seg.start {
						out.push(Node::Text(self.full.slice(seg.start as usize..seg.end as usize)));
					}
				},
				SegKind::Expr => {
					let toks = lex::tokenize(self.name, self.source, seg.start, seg.end)?;
					out.push(Node::Emit(self.expr(&toks, seg.span())?));
				},
				SegKind::Stmt => {
					if let Some(end) = self.statement(&seg, out)? {
						return Ok(Some(end));
					}
				},
			}
		}
		Ok(None)
	}

	/// Parses one statement tag; terminators bubble up as `Some(End)`.
	fn statement(&mut self, seg: &Segment, out: &mut Vec<Node>) -> Result<Option<End>, Error> {
		let toks = lex::tokenize(self.name, self.source, seg.start, seg.end)?;
		let Some(first) = toks.first() else {
			return Err(self.syntax(seg.span(), SyntaxErrorKind::UnexpectedEnd));
		};
		if first.kind != TokKind::Ident {
			return Err(self.syntax(first.span, SyntaxErrorKind::ExpectedIdent));
		}
		let keyword = self.text(first.span);
		match keyword {
			"if" => {
				out.push(self.parse_if(seg, &toks)?);
				Ok(None)
			},
			"for" => {
				out.push(self.parse_for(seg, &toks)?);
				Ok(None)
			},
			"set" => {
				out.push(self.parse_set(seg, &toks)?);
				Ok(None)
			},
			"elif" | "else" | "endif" | "endfor" => Ok(Some(End {
				keyword: self.slice(first.span),
				span:    first.span,
				toks:    toks[1..].to_vec(),
			})),
			word if word.starts_with("end") => {
				self.expect_bare(&toks)?;
				Ok(Some(End {
					keyword: self.slice(first.span),
					span:    first.span,
					toks:    Vec::new(),
				}))
			},
			"raw" | "endraw" => {
				// The lexer rewrites raw framing to markers; reaching here
				// means a malformed tag such as `{% raw x %}`.
				Err(self.syntax(first.span, SyntaxErrorKind::UnexpectedToken))
			},
			_ => {
				out.push(self.parse_block(seg, &toks, first.span)?);
				Ok(None)
			},
		}
	}

	fn parse_if(&mut self, seg: &Segment, toks: &[Tok]) -> Result<Node, Error> {
		let mut arms = Vec::new();
		let mut next_cond = Some(self.expr(&toks[1..], seg.span())?);
		loop {
			let mut body = Vec::new();
			let Some(end) = self.build(&mut body)? else {
				return Err(self.syntax(seg.span(), SyntaxErrorKind::UnclosedBlock));
			};
			arms.push(IfArm { cond: next_cond.take(), body });
			match end.keyword.as_str() {
				"elif" => next_cond = Some(self.expr(&end.toks, end.span)?),
				"else" => {
					if !end.toks.is_empty() {
						return Err(self.syntax(end.toks[0].span, SyntaxErrorKind::UnexpectedToken));
					}
					let mut body = Vec::new();
					let Some(end) = self.build(&mut body)? else {
						return Err(self.syntax(seg.span(), SyntaxErrorKind::UnclosedBlock));
					};
					if end.keyword != "endif" || !end.toks.is_empty() {
						return Err(self.syntax(end.span, SyntaxErrorKind::MismatchedEnd));
					}
					arms.push(IfArm { cond: None, body });
					break;
				},
				"endif" => {
					if !end.toks.is_empty() {
						return Err(self.syntax(end.toks[0].span, SyntaxErrorKind::UnexpectedToken));
					}
					break;
				},
				_ => return Err(self.syntax(end.span, SyntaxErrorKind::MismatchedEnd)),
			}
		}
		Ok(Node::If(arms))
	}

	fn parse_for(&mut self, seg: &Segment, toks: &[Tok]) -> Result<Node, Error> {
		let var = self.ident(toks.get(1), seg)?;
		match toks.get(2) {
			Some(tok) if tok.kind == TokKind::Ident && self.text(tok.span) == "in" => {},
			Some(tok) => return Err(self.syntax(tok.span, SyntaxErrorKind::UnexpectedToken)),
			None => return Err(self.syntax(seg.span(), SyntaxErrorKind::UnexpectedEnd)),
		}
		let iter = self.expr(&toks[3..], seg.span())?;
		let mut body = Vec::new();
		let Some(end) = self.build(&mut body)? else {
			return Err(self.syntax(seg.span(), SyntaxErrorKind::UnclosedBlock));
		};
		if end.keyword != "endfor" || !end.toks.is_empty() {
			return Err(self.syntax(end.span, SyntaxErrorKind::MismatchedEnd));
		}
		Ok(Node::For { var, iter, body, span: seg.span() })
	}

	fn parse_set(&self, seg: &Segment, toks: &[Tok]) -> Result<Node, Error> {
		let name = self.ident(toks.get(1), seg)?;
		match toks.get(2) {
			Some(tok) if tok.kind == TokKind::Assign => {},
			Some(tok) => return Err(self.syntax(tok.span, SyntaxErrorKind::UnexpectedToken)),
			None => return Err(self.syntax(seg.span(), SyntaxErrorKind::UnexpectedEnd)),
		}
		let value = self.expr(&toks[3..], seg.span())?;
		Ok(Node::Set { name, value })
	}

	fn parse_block(&mut self, seg: &Segment, toks: &[Tok], name_span: Span) -> Result<Node, Error> {
		let name = self.slice(name_span);
		let mut args = Vec::new();
		if toks.len() > 1 {
			let mut parser =
				ExprParser { builder: self, toks: &toks[1..], pos: 0, tag: seg.span() };
			loop {
				args.push(parser.parse_expr()?);
				match parser.peek() {
					Some(tok) if tok.kind == TokKind::Comma => parser.pos += 1,
					Some(tok) => {
						return Err(Error::syntax(
							self.name,
							self.source,
							tok.span,
							SyntaxErrorKind::UnexpectedToken,
						));
					},
					None => break,
				}
			}
		}
		let mut body = Vec::new();
		let Some(end) = self.build(&mut body)? else {
			return Err(self.syntax(seg.span(), SyntaxErrorKind::UnclosedBlock));
		};
		let matches = end
			.keyword
			.strip_prefix("end")
			.is_some_and(|rest| rest == name.as_str());
		if !matches || !end.toks.is_empty() {
			return Err(self.syntax(end.span, SyntaxErrorKind::MismatchedEnd));
		}
		Ok(Node::Block { name, name_span, args, body })
	}

	/// A required bare identifier (not reserved).
	fn ident(&self, tok: Option<&Tok>, seg: &Segment) -> Result<Str, Error> {
		match tok {
			Some(tok) if tok.kind == TokKind::Ident => {
				let text = self.text(tok.span);
				if RESERVED.contains(&text) || matches!(text, "true" | "false" | "none") {
					return Err(self.syntax(tok.span, SyntaxErrorKind::ExpectedIdent));
				}
				Ok(self.slice(tok.span))
			},
			Some(tok) => Err(self.syntax(tok.span, SyntaxErrorKind::ExpectedIdent)),
			None => Err(self.syntax(seg.span(), SyntaxErrorKind::UnexpectedEnd)),
		}
	}

	/// Terminators such as `{% endxml %}` carry no further tokens.
	fn expect_bare(&self, toks: &[Tok]) -> Result<(), Error> {
		match toks.get(1) {
			None => Ok(()),
			Some(tok) => Err(self.syntax(tok.span, SyntaxErrorKind::UnexpectedToken)),
		}
	}

	/// Parses a full expression that must consume every token.
	fn expr(&self, toks: &[Tok], tag: Span) -> Result<Expr, Error> {
		let mut parser = ExprParser { builder: self, toks, pos: 0, tag };
		let expr = parser.parse_expr()?;
		match parser.peek() {
			None => Ok(expr),
			Some(tok) => Err(self.syntax(tok.span, SyntaxErrorKind::UnexpectedToken)),
		}
	}
}

struct ExprParser<'p, 'b> {
	builder: &'p Builder<'b>,
	toks:    &'p [Tok],
	pos:     usize,
	/// Enclosing tag span, blamed when tokens run out.
	tag:     Span,
}

impl ExprParser<'_, '_> {
	fn peek(&self) -> Option<&Tok> {
		self.toks.get(self.pos)
	}

	fn next(&mut self) -> Result<Tok, Error> {
		let tok = self.toks.get(self.pos).ok_or_else(|| {
			self
				.builder
				.syntax(self.tag, SyntaxErrorKind::UnexpectedEnd)
		})?;
		self.pos += 1;
		Ok(tok.clone())
	}

	fn text(&self, tok: &Tok) -> &str {
		self.builder.text(tok.span)
	}

	/// Consumes an ident with exactly `keyword` text, if present.
	fn eat_keyword(&mut self, keyword: &str) -> bool {
		match self.peek() {
			Some(tok) if tok.kind == TokKind::Ident && self.text(tok) == keyword => {
				self.pos += 1;
				true
			},
			_ => false,
		}
	}

	fn parse_expr(&mut self) -> Result<Expr, Error> {
		self.parse_ternary()
	}

	fn parse_ternary(&mut self) -> Result<Expr, Error> {
		let then = self.parse_or()?;
		if !self.eat_keyword("if") {
			return Ok(then);
		}
		let cond = self.parse_or()?;
		if !self.eat_keyword("else") {
			return Err(match self.peek() {
				Some(tok) => self
					.builder
					.syntax(tok.span, SyntaxErrorKind::UnexpectedToken),
				None => self
					.builder
					.syntax(self.tag, SyntaxErrorKind::UnexpectedEnd),
			});
		}
		let otherwise = self.parse_ternary()?;
		Ok(Expr::Ternary {
			cond:      Box::new(cond),
			then:      Box::new(then),
			otherwise: Box::new(otherwise),
		})
	}

	fn parse_or(&mut self) -> Result<Expr, Error> {
		let mut lhs = self.parse_and()?;
		while let Some(span) = self.eat_keyword_span("or") {
			let rhs = self.parse_and()?;
			lhs = Expr::Bin { op: BinOp::Or, lhs: Box::new(lhs), rhs: Box::new(rhs), span };
		}
		Ok(lhs)
	}

	fn parse_and(&mut self) -> Result<Expr, Error> {
		let mut lhs = self.parse_not()?;
		while let Some(span) = self.eat_keyword_span("and") {
			let rhs = self.parse_not()?;
			lhs = Expr::Bin { op: BinOp::And, lhs: Box::new(lhs), rhs: Box::new(rhs), span };
		}
		Ok(lhs)
	}

	fn eat_keyword_span(&mut self, keyword: &str) -> Option<Span> {
		let span = self.peek().map(|tok| tok.span)?;
		self.eat_keyword(keyword).then_some(span)
	}

	fn parse_not(&mut self) -> Result<Expr, Error> {
		if let Some(_span) = self.eat_keyword_span("not") {
			return Ok(Expr::Not(Box::new(self.parse_not()?)));
		}
		self.parse_comparison()
	}

	fn parse_comparison(&mut self) -> Result<Expr, Error> {
		let mut lhs = self.parse_additive()?;
		while let Some(tok) = self.peek() {
			let op = match &tok.kind {
				TokKind::EqEq => BinOp::Eq,
				TokKind::Ne => BinOp::Ne,
				TokKind::Lt => BinOp::Lt,
				TokKind::Le => BinOp::Le,
				TokKind::Gt => BinOp::Gt,
				TokKind::Ge => BinOp::Ge,
				TokKind::Ident if self.text(tok) == "in" => BinOp::In,
				_ => break,
			};
			let span = self.next()?.span;
			let rhs = self.parse_additive()?;
			lhs = Expr::Bin { op, lhs: Box::new(lhs), rhs: Box::new(rhs), span };
		}
		Ok(lhs)
	}

	fn parse_additive(&mut self) -> Result<Expr, Error> {
		let mut lhs = self.parse_unary()?;
		loop {
			let op = match self.peek().map(|tok| &tok.kind) {
				Some(TokKind::Tilde) => BinOp::Concat,
				Some(TokKind::Plus) => BinOp::Add,
				Some(TokKind::Minus) => BinOp::Sub,
				_ => break,
			};
			let span = self.next()?.span;
			let rhs = self.parse_unary()?;
			lhs = Expr::Bin { op, lhs: Box::new(lhs), rhs: Box::new(rhs), span };
		}
		Ok(lhs)
	}

	fn parse_unary(&mut self) -> Result<Expr, Error> {
		if let Some(tok) = self.peek()
			&& tok.kind == TokKind::Minus
		{
			let span = tok.span;
			self.pos += 1;
			return Ok(Expr::Neg(Box::new(self.parse_unary()?), span));
		}
		self.parse_postfix()
	}

	fn parse_postfix(&mut self) -> Result<Expr, Error> {
		let mut expr = self.parse_primary()?;
		let mut lenient = false;
		loop {
			match self.peek().map(|tok| tok.kind.clone()) {
				Some(TokKind::Dot | TokKind::QDot) => {
					let optional = self.next()?.kind == TokKind::QDot;
					lenient |= optional;
					let tok = self.next()?;
					if tok.kind != TokKind::Ident {
						return Err(
							self
								.builder
								.syntax(tok.span, SyntaxErrorKind::ExpectedIdent),
						);
					}
					let span = tok.span;
					expr = Expr::Attr {
						base: Box::new(expr),
						name: self.builder.slice(span),
						optional: lenient,
						span,
					};
				},
				Some(TokKind::LBracket) => {
					let span = self.next()?.span;
					let index = self.parse_expr()?;
					let close = self.next()?;
					if close.kind != TokKind::RBracket {
						return Err(
							self
								.builder
								.syntax(close.span, SyntaxErrorKind::UnexpectedToken),
						);
					}
					expr = Expr::Index {
						base: Box::new(expr),
						index: Box::new(index),
						optional: lenient,
						span,
					};
				},
				Some(TokKind::Pipe) => {
					self.pos += 1;
					let tok = self.next()?;
					if tok.kind != TokKind::Ident {
						return Err(
							self
								.builder
								.syntax(tok.span, SyntaxErrorKind::ExpectedIdent),
						);
					}
					let name_span = tok.span;
					let args = self.parse_call_args()?;
					expr = Expr::Filter {
						name: self.builder.slice(name_span),
						name_span,
						input: Box::new(expr),
						args,
					};
					lenient = false;
				},
				_ => break,
			}
		}
		Ok(expr)
	}

	/// Parenthesized argument list after a filter or function name; absent
	/// parentheses mean no arguments.
	fn parse_call_args(&mut self) -> Result<Vec<Expr>, Error> {
		let mut args = Vec::new();
		if !matches!(self.peek().map(|tok| &tok.kind), Some(TokKind::LParen)) {
			return Ok(args);
		}
		self.pos += 1;
		if matches!(self.peek().map(|tok| &tok.kind), Some(TokKind::RParen)) {
			self.pos += 1;
			return Ok(args);
		}
		loop {
			args.push(self.parse_expr()?);
			let tok = self.next()?;
			match tok.kind {
				TokKind::Comma => {},
				TokKind::RParen => break,
				_ => {
					return Err(
						self
							.builder
							.syntax(tok.span, SyntaxErrorKind::UnexpectedToken),
					);
				},
			}
		}
		Ok(args)
	}

	fn parse_primary(&mut self) -> Result<Expr, Error> {
		let tok = self.next()?;
		let span = tok.span;
		match tok.kind {
			TokKind::Int(value) => Ok(Expr::Lit(Value::Int(value))),
			TokKind::Float(value) => Ok(Expr::Lit(Value::Float(value))),
			TokKind::Str(value) => Ok(Expr::Lit(Value::Str(value))),
			TokKind::LParen => {
				let expr = self.parse_expr()?;
				let close = self.next()?;
				if close.kind != TokKind::RParen {
					return Err(
						self
							.builder
							.syntax(close.span, SyntaxErrorKind::UnexpectedToken),
					);
				}
				Ok(expr)
			},
			TokKind::Ident => match self.builder.text(span) {
				"true" => Ok(Expr::Lit(Value::Bool(true))),
				"false" => Ok(Expr::Lit(Value::Bool(false))),
				"none" => Ok(Expr::Lit(Value::None)),
				text if RESERVED.contains(&text) => {
					Err(self.builder.syntax(span, SyntaxErrorKind::UnexpectedToken))
				},
				_ => {
					if matches!(self.peek().map(|tok| &tok.kind), Some(TokKind::LParen)) {
						let args = self.parse_call_args()?;
						return Ok(Expr::Call { name: self.builder.slice(span), name_span: span, args });
					}
					Ok(Expr::Var { name: self.builder.slice(span), span })
				},
			},
			_ => Err(self.builder.syntax(span, SyntaxErrorKind::UnexpectedToken)),
		}
	}
}
