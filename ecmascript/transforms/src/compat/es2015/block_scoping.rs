use crate::{pass::Pass, util::undefined};
use smallvec::SmallVec;
use std::mem::replace;
use swc_common::{util::map::Map, Fold, FoldWith, Spanned, Visit, VisitWith, DUMMY_SP};
use swc_ecma_ast::*;
use swc_ecma_utils::{
    find_ids, ident::IdentLike, prepend, var::VarCollector, ExprFactory, Id, StmtLike,
};

///
///
/// TODO(kdy1): Optimization
///
/// ```js
/// let functions = [];
/// for (let i = 0; i < 10; i++) {
/// 	functions.push(function() {
///        let i = 1;
/// 		console.log(i);
/// 	});
/// }
/// ```
pub fn block_scoping() -> impl Pass {
    BlockScoping {
        scope: Default::default(),
        vars: vec![],
        var_decl_kind: VarDeclKind::Var,
    }
}

type ScopeStack = SmallVec<[ScopeKind; 8]>;

#[derive(Debug, PartialEq, Eq)]
enum ScopeKind {
    Loop,
    ForLetLoop {
        all: Vec<Id>,
        args: Vec<Id>,
        /// Produced by identifier reference and consumed by for-of/in loop.
        used: Vec<Id>,
    },
    Fn,
    Block,
}

struct BlockScoping {
    scope: ScopeStack,
    vars: Vec<VarDeclarator>,
    var_decl_kind: VarDeclKind,
}

noop_fold_type!(BlockScoping);

impl BlockScoping {
    /// This methods remove [ScopeKind::Loop] and [ScopeKind::Fn], but not
    /// [ScopeKind::ForLetLoop]
    fn fold_with_scope<T>(&mut self, kind: ScopeKind, node: T) -> T
    where
        T: FoldWith<Self>,
    {
        let remove = match kind {
            ScopeKind::ForLetLoop { .. } => false,
            _ => true,
        };
        self.scope.push(kind);
        let node = node.fold_with(self);

        if remove {
            self.scope.pop();
        }

        node
    }

    fn mark_as_used(&mut self, i: Id) {
        for (idx, scope) in self.scope.iter_mut().rev().enumerate() {
            match scope {
                ScopeKind::ForLetLoop { all, used, .. } => {
                    //
                    if all.contains(&i) {
                        if idx == 0 {
                            return;
                        }

                        used.push(i);
                        return;
                    }
                }
                _ => {}
            }
        }
    }

    fn in_loop_body(&self) -> bool {
        self.scope
            .last()
            .map(|scope| match scope {
                ScopeKind::ForLetLoop { .. } | ScopeKind::Loop => true,
                _ => false,
            })
            .unwrap_or(false)
    }

    fn handle_vars(&mut self, body: Box<Stmt>) -> Box<Stmt> {
        body.map(|body| {
            {
                let mut v = FunctionFinder { found: false };
                body.visit_with(&mut v);
                if !v.found {
                    return body;
                }
            }

            //
            if let Some(ScopeKind::ForLetLoop { args, used, .. }) = self.scope.pop() {
                if used.is_empty() {
                    return body;
                }
                let mut flow_helper = FlowHelper {
                    has_continue: false,
                    has_break: false,
                    has_return: false,
                };

                let var_name = private_ident!("_loop");

                self.vars.push(VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(var_name.clone()),
                    init: Some(
                        box FnExpr {
                            ident: None,
                            function: Function {
                                span: DUMMY_SP,
                                params: args
                                    .iter()
                                    .map(|i| Param {
                                        span: DUMMY_SP,
                                        decorators: Default::default(),
                                        pat: Pat::Ident(Ident::new(
                                            i.0.clone(),
                                            DUMMY_SP.with_ctxt(i.1),
                                        )),
                                    })
                                    .collect(),
                                decorators: Default::default(),
                                body: Some(match body.fold_with(&mut flow_helper) {
                                    Stmt::Block(bs) => bs,
                                    body => BlockStmt {
                                        span: DUMMY_SP,
                                        stmts: vec![body],
                                    },
                                }),
                                is_generator: false,
                                is_async: false,
                                type_params: None,
                                return_type: None,
                            },
                        }
                        .into(),
                    ),
                    definite: false,
                });

                let call = CallExpr {
                    span: DUMMY_SP,
                    callee: var_name.as_callee(),
                    args: args
                        .into_iter()
                        .map(|i| ExprOrSpread {
                            spread: None,
                            expr: box Expr::Ident(Ident::new(i.0, DUMMY_SP.with_ctxt(i.1))),
                        })
                        .collect(),
                    type_args: None,
                };

                if flow_helper.has_return || flow_helper.has_continue || flow_helper.has_break {
                    let ret = private_ident!("_ret");

                    let mut stmts = vec![
                        // var _ret = _loop(i);
                        Stmt::Decl(Decl::Var(VarDecl {
                            span: DUMMY_SP,
                            kind: VarDeclKind::Var,
                            declare: false,
                            decls: vec![VarDeclarator {
                                span: DUMMY_SP,
                                name: Pat::Ident(ret.clone()),
                                init: Some(box call.into()),
                                definite: false,
                            }],
                        })),
                    ];

                    let use_switch = flow_helper.has_break && flow_helper.has_continue;

                    let check_ret = if flow_helper.has_return {
                        // if (_typeof(_ret) === "object") return _ret.v;
                        Some(
                            IfStmt {
                                span: DUMMY_SP,
                                test: box Expr::Bin(BinExpr {
                                    span: DUMMY_SP,
                                    op: BinaryOp::EqEqEq,
                                    left: {
                                        // _typeof(_ret)
                                        let callee = helper!(type_of, "typeof");

                                        box Expr::Call(CallExpr {
                                            span: Default::default(),
                                            callee,
                                            args: vec![ExprOrSpread {
                                                spread: None,
                                                expr: box ret.clone().into(),
                                            }],
                                            type_args: None,
                                        })
                                    },
                                    //"object"
                                    right: box Expr::Lit(Lit::Str(Str {
                                        span: DUMMY_SP,
                                        value: js_word!("object"),
                                        has_escape: false,
                                    })),
                                }),
                                cons: box Stmt::Return(ReturnStmt {
                                    span: DUMMY_SP,
                                    arg: Some(box ret.clone().member(quote_ident!("v"))),
                                }),
                                alt: None,
                            }
                            .into(),
                        )
                    } else {
                        None
                    };

                    if use_switch {
                        let mut cases = vec![];

                        if flow_helper.has_break {
                            cases.push(
                                SwitchCase {
                                    span: DUMMY_SP,
                                    test: Some(box quote_str!("break").into()),
                                    // TODO: Handle labelled statements
                                    cons: vec![Stmt::Break(BreakStmt {
                                        span: DUMMY_SP,
                                        label: None,
                                    })],
                                }
                                .into(),
                            );
                        }

                        if flow_helper.has_continue {
                            cases.push(
                                SwitchCase {
                                    span: DUMMY_SP,
                                    test: Some(box quote_str!("continue").into()),
                                    // TODO: Handle labelled statements
                                    cons: vec![Stmt::Continue(ContinueStmt {
                                        span: DUMMY_SP,
                                        label: None,
                                    })],
                                }
                                .into(),
                            );
                        }

                        cases.extend(check_ret.map(|stmt| SwitchCase {
                            span: DUMMY_SP,
                            test: None,
                            cons: vec![stmt],
                        }));

                        stmts.push(
                            SwitchStmt {
                                span: DUMMY_SP,
                                discriminant: box ret.clone().into(),
                                cases,
                            }
                            .into(),
                        );
                    } else {
                        //
                        if flow_helper.has_break {
                            stmts.push(
                                IfStmt {
                                    span: DUMMY_SP,
                                    test: box ret.clone().make_eq(quote_str!("break")),
                                    // TODO: Handle labelled statements
                                    cons: box Stmt::Break(BreakStmt {
                                        span: DUMMY_SP,
                                        label: None,
                                    }),
                                    alt: None,
                                }
                                .into(),
                            );
                        }

                        if flow_helper.has_continue {
                            stmts.push(
                                IfStmt {
                                    span: DUMMY_SP,
                                    test: box ret.clone().make_eq(quote_str!("continue")),
                                    // TODO: Handle labelled statements
                                    cons: box Stmt::Continue(ContinueStmt {
                                        span: DUMMY_SP,
                                        label: None,
                                    }),
                                    alt: None,
                                }
                                .into(),
                            );
                        }

                        stmts.extend(check_ret);
                    }

                    return BlockStmt {
                        span: DUMMY_SP,
                        stmts,
                    }
                    .into();
                }

                return call.into_stmt();
            }

            body
        })
    }
}

impl Fold<DoWhileStmt> for BlockScoping {
    fn fold(&mut self, node: DoWhileStmt) -> DoWhileStmt {
        let body = self.fold_with_scope(ScopeKind::Loop, node.body);

        let test = node.test.fold_with(self);

        DoWhileStmt { body, test, ..node }
    }
}

impl Fold<WhileStmt> for BlockScoping {
    fn fold(&mut self, node: WhileStmt) -> WhileStmt {
        let body = self.fold_with_scope(ScopeKind::Loop, node.body);

        let test = node.test.fold_with(self);

        WhileStmt { body, test, ..node }
    }
}

impl Fold<ForStmt> for BlockScoping {
    fn fold(&mut self, node: ForStmt) -> ForStmt {
        let init = node.init.fold_with(self);

        let mut vars = find_vars(&init);
        let args = vars.clone();

        let test = node.test.fold_with(self);
        let update = node.update.fold_with(self);

        find_infected(&mut vars, &node.body);

        let kind = if vars.is_empty() {
            ScopeKind::Loop
        } else {
            ScopeKind::ForLetLoop {
                all: vars,
                args,
                used: vec![],
            }
        };
        let body = self.fold_with_scope(kind, node.body);
        let body = self.handle_vars(body);

        ForStmt {
            init,
            test,
            update,
            body,
            ..node
        }
    }
}

impl Fold<ForOfStmt> for BlockScoping {
    fn fold(&mut self, node: ForOfStmt) -> ForOfStmt {
        let left = self.fold_with_scope(ScopeKind::Block, node.left);
        let mut vars = find_vars(&left);
        let args = vars.clone();

        let right = node.right.fold_with(self);

        find_infected(&mut vars, &node.body);

        let kind = if vars.is_empty() {
            ScopeKind::Loop
        } else {
            ScopeKind::ForLetLoop {
                all: vars,
                args,
                used: vec![],
            }
        };
        let body = self.fold_with_scope(kind, node.body);
        let body = self.handle_vars(body);

        ForOfStmt {
            left,
            right,
            body,
            ..node
        }
    }
}

impl Fold<ForInStmt> for BlockScoping {
    fn fold(&mut self, node: ForInStmt) -> ForInStmt {
        let left = self.fold_with_scope(ScopeKind::Block, node.left);
        let mut vars = find_vars(&left);
        let args = vars.clone();

        let right = node.right.fold_with(self);

        find_infected(&mut vars, &node.body);

        let kind = if vars.is_empty() {
            ScopeKind::Loop
        } else {
            ScopeKind::ForLetLoop {
                all: vars,
                args,
                used: vec![],
            }
        };
        let body = self.fold_with_scope(kind, node.body);
        let body = self.handle_vars(body);

        ForInStmt {
            left,
            right,
            body,
            ..node
        }
    }
}

impl Fold<Function> for BlockScoping {
    fn fold(&mut self, f: Function) -> Function {
        Function {
            params: f.params.fold_with(self),
            decorators: f.decorators.fold_with(self),
            body: self.fold_with_scope(ScopeKind::Fn, f.body),
            ..f
        }
    }
}

impl Fold<ArrowExpr> for BlockScoping {
    fn fold(&mut self, f: ArrowExpr) -> ArrowExpr {
        ArrowExpr {
            params: f.params.fold_with(self),
            body: self.fold_with_scope(ScopeKind::Fn, f.body),
            ..f
        }
    }
}

impl Fold<Constructor> for BlockScoping {
    fn fold(&mut self, f: Constructor) -> Constructor {
        Constructor {
            key: f.key.fold_with(self),
            params: f.params.fold_with(self),
            body: self.fold_with_scope(ScopeKind::Fn, f.body),
            ..f
        }
    }
}

impl Fold<GetterProp> for BlockScoping {
    fn fold(&mut self, f: GetterProp) -> GetterProp {
        GetterProp {
            key: f.key.fold_with(self),
            body: self.fold_with_scope(ScopeKind::Fn, f.body),
            ..f
        }
    }
}

impl Fold<SetterProp> for BlockScoping {
    fn fold(&mut self, f: SetterProp) -> SetterProp {
        SetterProp {
            key: f.key.fold_with(self),
            param: f.param.fold_with(self),
            body: self.fold_with_scope(ScopeKind::Fn, f.body),
            ..f
        }
    }
}

impl Fold<VarDecl> for BlockScoping {
    fn fold(&mut self, var: VarDecl) -> VarDecl {
        let old = self.var_decl_kind;
        self.var_decl_kind = var.kind;
        let var = var.fold_children(self);

        self.var_decl_kind = old;

        VarDecl {
            kind: VarDeclKind::Var,
            ..var
        }
    }
}

impl Fold<VarDeclarator> for BlockScoping {
    fn fold(&mut self, var: VarDeclarator) -> VarDeclarator {
        let var = var.fold_children(self);

        let init = if self.in_loop_body() && var.init.is_none() {
            if self.var_decl_kind == VarDeclKind::Var {
                None
            } else {
                Some(undefined(var.span()))
            }
        } else {
            var.init
        };

        VarDeclarator { init, ..var }
    }
}

impl Fold<Ident> for BlockScoping {
    fn fold(&mut self, node: Ident) -> Ident {
        let id = node.to_id();
        self.mark_as_used(id);

        node
    }
}

impl<T> Fold<Vec<T>> for BlockScoping
where
    T: StmtLike,
    Vec<T>: FoldWith<Self>,
{
    fn fold(&mut self, stmts: Vec<T>) -> Vec<T> {
        let mut stmts = stmts.fold_children(self);

        if !self.vars.is_empty() {
            prepend(
                &mut stmts,
                T::from_stmt(Stmt::Decl(Decl::Var(VarDecl {
                    span: DUMMY_SP,
                    kind: VarDeclKind::Var,
                    declare: false,
                    decls: replace(&mut self.vars, Default::default()),
                }))),
            );
        }
        stmts
    }
}

fn find_vars<T>(node: &T) -> Vec<Id>
where
    T: for<'any> VisitWith<VarCollector<'any>>,
{
    let mut vars = vec![];
    let mut v = VarCollector { to: &mut vars };
    node.visit_with(&mut v);

    vars
}

fn find_infected<T>(ids: &mut Vec<Id>, node: &T)
where
    T: for<'any> VisitWith<InfectionFinder<'any>>,
{
    let mut v = InfectionFinder {
        vars: ids,
        found: false,
    };
    node.visit_with(&mut v);
}

/// In the code below,
///
/// ```js
/// let i = _step.value
/// ```
///
/// `i` is infected by `_step`.
struct InfectionFinder<'a> {
    vars: &'a mut Vec<Id>,
    found: bool,
}

noop_visit_type!(InfectionFinder<'_>);

impl Visit<VarDeclarator> for InfectionFinder<'_> {
    fn visit(&mut self, node: &VarDeclarator) {
        let old = self.found;
        self.found = false;

        node.init.visit_with(self);

        if self.found {
            let ids = find_ids(&node.name);
            self.vars.extend(ids);
        }

        self.found = old;
    }
}

impl Visit<AssignExpr> for InfectionFinder<'_> {
    fn visit(&mut self, node: &AssignExpr) {
        let old = self.found;
        self.found = false;

        node.right.visit_with(self);

        if self.found {
            let ids = find_ids(&node.left);
            self.vars.extend(ids);
        }

        self.found = old;
    }
}

impl Visit<MemberExpr> for InfectionFinder<'_> {
    fn visit(&mut self, e: &MemberExpr) {
        if self.found {
            return;
        }

        e.obj.visit_with(self);

        if e.computed {
            e.prop.visit_with(self);
        }
    }
}

impl Visit<Ident> for InfectionFinder<'_> {
    fn visit(&mut self, i: &Ident) {
        if self.found {
            return;
        }

        for ident in &*self.vars {
            if i.span.ctxt() == ident.1 && i.sym == ident.0 {
                self.found = true;
                break;
            }
        }
    }
}

#[derive(Debug)]
struct FlowHelper {
    has_continue: bool,
    has_break: bool,
    has_return: bool,
}

noop_fold_type!(FlowHelper);

/// noop
impl Fold<Function> for FlowHelper {
    fn fold(&mut self, f: Function) -> Function {
        f
    }
}

impl Fold<ArrowExpr> for FlowHelper {
    fn fold(&mut self, f: ArrowExpr) -> ArrowExpr {
        f
    }
}

impl Fold<Stmt> for FlowHelper {
    fn fold(&mut self, node: Stmt) -> Stmt {
        let span = node.span();

        match node {
            Stmt::Continue(..) => {
                self.has_continue = true;
                return Stmt::Return(ReturnStmt {
                    span,
                    arg: Some(box Expr::Lit(Lit::Str(Str {
                        span,
                        value: "continue".into(),
                        has_escape: false,
                    }))),
                });
            }
            Stmt::Break(..) => {
                self.has_break = true;
                return Stmt::Return(ReturnStmt {
                    span,
                    arg: Some(box Expr::Lit(Lit::Str(Str {
                        span,
                        value: "break".into(),
                        has_escape: false,
                    }))),
                });
            }
            Stmt::Return(s) => {
                self.has_return = true;
                let s: ReturnStmt = s.fold_with(self);

                return Stmt::Return(ReturnStmt {
                    span,
                    arg: Some(box Expr::Object(ObjectLit {
                        span,
                        props: vec![PropOrSpread::Prop(box Prop::KeyValue(KeyValueProp {
                            key: PropName::Ident(Ident::new("v".into(), DUMMY_SP)),
                            value: s.arg.unwrap_or_else(|| {
                                box Expr::Unary(UnaryExpr {
                                    span: DUMMY_SP,
                                    op: UnaryOp::Void,
                                    arg: undefined(DUMMY_SP),
                                })
                            }),
                        }))],
                    })),
                });
            }
            _ => node.fold_children(self),
        }
    }
}

#[derive(Debug)]
struct FunctionFinder {
    found: bool,
}

noop_visit_type!(FunctionFinder);

impl Visit<Function> for FunctionFinder {
    fn visit(&mut self, _: &Function) {
        self.found = true
    }
}

#[cfg(test)]
mod tests {
    use super::block_scoping;
    use crate::compat::{es2015, es2015::for_of::for_of};
    use swc_common::{chain, Mark};

    test!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        for_loop,
        "for (const key in obj) {
            const bar = obj[key];

            let qux;
            let fog;

            if (Array.isArray(bar)) {
            qux = bar[0];
            fog = bar[1];
            } else {
            qux = bar;
            }

            baz(key, qux, fog);
        }",
        "for (var key in obj) {
            var bar = obj[key];

            var qux = void 0;
            var fog = void 0;

            if (Array.isArray(bar)) {
            qux = bar[0];
            fog = bar[1];
            } else {
            qux = bar;
            }

            baz(key, qux, fog);
        }"
    );

    test!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        for_let_loop,
        "let functions = [];
for (let i = 0; i < 10; i++) {
	functions.push(function() {
		console.log(i);
	});
}
functions[0]();
functions[7]();",
        "
var _loop = function(i) {
    functions.push(function() {
        console.log(i);
    });
};
var functions = [];
for(var i = 0; i < 10; i++)_loop(i);
functions[0]();
functions[7]();
"
    );

    test_exec!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        for_let_loop_exec,
        "let functions = [];
for (let i = 0; i < 10; i++) {
	functions.push(function() {
		return i;
	});
}
expect(functions[0]()).toBe(0);
expect(functions[7]()).toBe(7);
"
    );

    test_exec!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        for_let_of_exec,
        "let functions = [];
for (let i of [1, 3, 5, 7, 9]) {
	functions.push(function() {
		return i;
	});
}
expect(functions[0]()).toBe(1);
expect(functions[1]()).toBe(3);
"
    );

    test_exec!(
        ::swc_ecma_parser::Syntax::default(),
        |_| chain!(for_of(Default::default()), block_scoping()),
        issue_609_1,
        "let functions = [];
for (let i of [1, 3, 5, 7, 9]) {
	functions.push(function() {
		return i;
	});
}
expect(functions[0]()).toBe(1);
expect(functions[1]()).toBe(3);
"
    );

    test!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        issue_662,
        "function foo(parts) {
  let match = null;

  for (let i = 1; i >= 0; i--) {
    for (let j = 0; j >= 0; j--) {
      match = parts[i][j];

      if (match) {
        break;
      }
    }

    if (match) {
      break;
    }
  }

  return match;
}

foo();",
        "function foo(parts) {
  var match = null;

  for (var i = 1; i >= 0; i--) {
    for (var j = 0; j >= 0; j--) {
      match = parts[i][j];

      if (match) {
        break;
      }
    }

    if (match) {
      break;
    }
  }

  return match;
}

foo();"
    );

    test!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        issue_686,
        "module.exports = function(values) {
    var vars = [];
    var elem = null, name, val;
    for (var i = 0; i < this.elements.length; i++) {
      elem = this.elements[i];
      name = elem.name;
      if (!name) continue;
      val = values[name];
      if (val == null) val = '';
      switch (elem.type) {
      case 'submit':
        break;
      case 'radio':
      case 'checkbox':
        elem.checked = val.some(function(str) {
          return str.toString() == elem.value;
        });
        break;
      case 'select-multiple':
        elem.fill(val);
        break;
      case 'textarea':
        elem.innerText = val;
        break;
      case 'hidden':
        break;
      default:
        if (elem.fill) {
          elem.fill(val);
        } else {
          elem.value = val;
        }
        break;
      }
    }
    return vars;
  };",
        "module.exports = function(values) {
    var _loop = function(i) {
        elem = this.elements[i];
        name = elem.name;
        if (!name) return 'continue';
        val = values[name];
        if (val == null) val = '';
        switch(elem.type){
            case 'submit':
                return 'break';
            case 'radio':
            case 'checkbox':
                elem.checked = val.some(function(str) {
                    return str.toString() == elem.value;
                });
                return 'break';
            case 'select-multiple':
                elem.fill(val);
                return 'break';
            case 'textarea':
                elem.innerText = val;
                return 'break';
            case 'hidden':
                return 'break';
            default:
                if (elem.fill) {
                    elem.fill(val);
                } else {
                    elem.value = val;
                }
                return 'break';
        }
    };
    var vars = [];
    var elem = null, name, val;
    for(var i = 0; i < this.elements.length; i++){
        var _ret = _loop(i);
        switch(_ret){
            case 'break':
                break;
            case 'continue':
                continue;
        }
    }
    return vars;
};"
    );

    test_exec!(
        ::swc_ecma_parser::Syntax::default(),
        |_| block_scoping(),
        issue_723_1,
        "function foo() {
  const lod = { 0: { mig: 'bana' }};

  for (let i = 0; i < 1; i++) {
    const { mig } = lod[i];

    return false;

    (zap) => zap === mig;
  }

  return true;
}
expect(foo()).toBe(false);
"
    );

    test_exec!(
        ::swc_ecma_parser::Syntax::default(),
        |_| {
            let mark = Mark::fresh(Mark::root());
            es2015::es2015(
                mark,
                es2015::Config {
                    ..Default::default()
                },
            )
        },
        issue_723_2,
        "function foo() {
  const lod = { 0: { mig: 'bana' }};

  for (let i = 0; i < 1; i++) {
    const { mig } = lod[i];

    return false;

    (zap) => zap === mig;
  }

  return true;
}
expect(foo()).toBe(false);
"
    );
}
