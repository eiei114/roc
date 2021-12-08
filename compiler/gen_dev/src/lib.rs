#![warn(clippy::dbg_macro)]
// See github.com/rtfeldman/roc/issues/800 for discussion of the large_enum_variant check.
#![allow(clippy::large_enum_variant, clippy::upper_case_acronyms)]

use bumpalo::{collections::Vec, Bump};
use roc_builtins::bitcode::{self, FloatWidth, IntWidth};
use roc_collections::all::{MutMap, MutSet};
use roc_module::ident::{ModuleName, TagName};
use roc_module::low_level::LowLevel;
use roc_module::symbol::{Interns, Symbol};
use roc_mono::ir::{
    BranchInfo, CallType, Expr, JoinPointId, ListLiteralElement, Literal, Param, Proc,
    SelfRecursive, Stmt,
};
use roc_mono::layout::{Builtin, Layout, LayoutIds};
use roc_reporting::internal_error;

mod generic64;
mod object_builder;
pub use object_builder::build_module;
mod run_roc;

pub struct Env<'a> {
    pub arena: &'a Bump,
    pub interns: Interns,
    pub exposed_to_host: MutSet<Symbol>,
    pub lazy_literals: bool,
    pub generate_allocators: bool,
}

// These relocations likely will need a length.
// They may even need more definition, but this should be at least good enough for how we will use elf.
#[derive(Debug)]
#[allow(dead_code)]
pub enum Relocation {
    LocalData {
        offset: u64,
        // This should probably technically be a bumpalo::Vec.
        // The problem is that it currently is built in a place that can't access the arena.
        data: std::vec::Vec<u8>,
    },
    LinkedFunction {
        offset: u64,
        name: String,
    },
    LinkedData {
        offset: u64,
        name: String,
    },
    JmpToReturn {
        inst_loc: u64,
        inst_size: u64,
        offset: u64,
    },
}

trait Backend<'a>
where
    Self: Sized,
{
    /// new creates a new backend that will output to the specific Object.
    fn new(env: &'a Env) -> Self;

    fn env(&self) -> &'a Env<'a>;

    /// reset resets any registers or other values that may be occupied at the end of a procedure.
    /// It also passes basic procedure information to the builder for setup of the next function.
    fn reset(&mut self, name: String, is_self_recursive: &'a SelfRecursive);

    /// finalize does any setup and cleanup that should happen around the procedure.
    /// finalize does setup because things like stack size and jump locations are not know until the function is written.
    /// For example, this can store the frame pointer and setup stack space.
    /// finalize is run at the end of build_proc when all internal code is finalized.
    fn finalize(&mut self) -> (&'a [u8], &[Relocation]);

    // load_args is used to let the backend know what the args are.
    // The backend should track these args so it can use them as needed.
    fn load_args(&mut self, args: &'a [(Layout<'a>, Symbol)], ret_layout: &Layout<'a>);

    /// Used for generating wrappers for malloc/realloc/free
    fn build_wrapped_jmp(&mut self) -> (&'a [u8], u64);

    /// build_proc creates a procedure and outputs it to the wrapped object writer.
    fn build_proc(&mut self, proc: &'a Proc<'a>) -> (&'a [u8], &[Relocation]) {
        let proc_name = LayoutIds::default()
            .get(proc.name, &proc.ret_layout)
            .to_symbol_string(proc.name, &self.env().interns);
        self.reset(proc_name, &proc.is_self_recursive);
        self.load_args(proc.args, &proc.ret_layout);
        for (layout, sym) in proc.args {
            self.set_layout_map(*sym, layout);
        }
        self.scan_ast(&proc.body);
        self.create_free_map();
        self.build_stmt(&proc.body, &proc.ret_layout);
        self.finalize()
    }

    /// build_stmt builds a statement and outputs at the end of the buffer.
    fn build_stmt(&mut self, stmt: &'a Stmt<'a>, ret_layout: &Layout<'a>) {
        match stmt {
            Stmt::Let(sym, expr, layout, following) => {
                self.build_expr(sym, expr, layout);
                self.set_layout_map(*sym, layout);
                self.free_symbols(stmt);
                self.build_stmt(following, ret_layout);
            }
            Stmt::Ret(sym) => {
                self.load_literal_symbols(&[*sym]);
                self.return_symbol(sym, ret_layout);
                self.free_symbols(stmt);
            }
            Stmt::Refcounting(_modify, following) => {
                // TODO: actually deal with refcounting. For hello world, we just skipped it.
                self.build_stmt(following, ret_layout);
            }
            Stmt::Switch {
                cond_symbol,
                cond_layout,
                branches,
                default_branch,
                ret_layout,
            } => {
                self.load_literal_symbols(&[*cond_symbol]);
                self.build_switch(
                    cond_symbol,
                    cond_layout,
                    branches,
                    default_branch,
                    ret_layout,
                );
                self.free_symbols(stmt);
            }
            Stmt::Join {
                id,
                parameters,
                body,
                remainder,
            } => {
                for param in parameters.iter() {
                    self.set_layout_map(param.symbol, &param.layout);
                }
                self.build_join(id, parameters, body, remainder, ret_layout);
                self.free_symbols(stmt);
            }
            Stmt::Jump(id, args) => {
                let mut arg_layouts: bumpalo::collections::Vec<Layout<'a>> =
                    bumpalo::vec![in self.env().arena];
                arg_layouts.reserve(args.len());
                let layout_map = self.layout_map();
                for arg in *args {
                    if let Some(layout) = layout_map.get(arg) {
                        arg_layouts.push(*layout);
                    } else {
                        internal_error!("the argument, {:?}, has no know layout", arg);
                    }
                }
                self.build_jump(id, args, arg_layouts.into_bump_slice(), ret_layout);
                self.free_symbols(stmt);
            }
            x => unimplemented!("the statement, {:?}, is not yet implemented", x),
        }
    }
    // build_switch generates a instructions for a switch statement.
    fn build_switch(
        &mut self,
        cond_symbol: &Symbol,
        cond_layout: &Layout<'a>,
        branches: &'a [(u64, BranchInfo<'a>, Stmt<'a>)],
        default_branch: &(BranchInfo<'a>, &'a Stmt<'a>),
        ret_layout: &Layout<'a>,
    );

    // build_join generates a instructions for a join statement.
    fn build_join(
        &mut self,
        id: &JoinPointId,
        parameters: &'a [Param<'a>],
        body: &'a Stmt<'a>,
        remainder: &'a Stmt<'a>,
        ret_layout: &Layout<'a>,
    );

    // build_jump generates a instructions for a jump statement.
    fn build_jump(
        &mut self,
        id: &JoinPointId,
        args: &'a [Symbol],
        arg_layouts: &[Layout<'a>],
        ret_layout: &Layout<'a>,
    );

    /// build_expr builds the expressions for the specified symbol.
    /// The builder must keep track of the symbol because it may be referred to later.
    fn build_expr(&mut self, sym: &Symbol, expr: &'a Expr<'a>, layout: &'a Layout<'a>) {
        match expr {
            Expr::Literal(lit) => {
                if self.env().lazy_literals {
                    self.literal_map().insert(*sym, (lit, layout));
                } else {
                    self.load_literal(sym, layout, lit);
                }
            }
            Expr::Call(roc_mono::ir::Call {
                call_type,
                arguments,
            }) => {
                match call_type {
                    CallType::ByName {
                        name: func_sym,
                        arg_layouts,
                        ret_layout,
                        ..
                    } => {
                        // If this function is just a lowlevel wrapper, then inline it
                        if let Some(lowlevel) = LowLevel::from_inlined_wrapper(*func_sym) {
                            self.build_run_low_level(
                                sym,
                                &lowlevel,
                                arguments,
                                arg_layouts,
                                ret_layout,
                            )
                        } else if func_sym
                            .module_string(&self.env().interns)
                            .starts_with(ModuleName::APP)
                        {
                            let fn_name = LayoutIds::default()
                                .get(*func_sym, layout)
                                .to_symbol_string(*func_sym, &self.env().interns);
                            // Now that the arguments are needed, load them if they are literals.
                            self.load_literal_symbols(arguments);
                            self.build_fn_call(sym, fn_name, arguments, arg_layouts, ret_layout)
                        } else {
                            self.build_inline_builtin(
                                sym,
                                *func_sym,
                                arguments,
                                arg_layouts,
                                ret_layout,
                            )
                        }
                    }

                    CallType::LowLevel { op: lowlevel, .. } => {
                        let mut arg_layouts: bumpalo::collections::Vec<Layout<'a>> =
                            bumpalo::vec![in self.env().arena];
                        arg_layouts.reserve(arguments.len());
                        let layout_map = self.layout_map();
                        for arg in *arguments {
                            if let Some(layout) = layout_map.get(arg) {
                                arg_layouts.push(*layout);
                            } else {
                                internal_error!("the argument, {:?}, has no know layout", arg);
                            }
                        }
                        self.build_run_low_level(
                            sym,
                            lowlevel,
                            arguments,
                            arg_layouts.into_bump_slice(),
                            layout,
                        )
                    }
                    x => unimplemented!("the call type, {:?}, is not yet implemented", x),
                }
            }
            Expr::Struct(fields) => {
                self.load_literal_symbols(fields);
                self.create_struct(sym, layout, fields);
            }
            Expr::StructAtIndex {
                index,
                field_layouts,
                structure,
            } => {
                self.load_struct_at_index(sym, structure, *index, field_layouts);
            }
            x => unimplemented!("the expression, {:?}, is not yet implemented", x),
        }
    }

    /// build_run_low_level builds the low level opertation and outputs to the specified symbol.
    /// The builder must keep track of the symbol because it may be referred to later.
    fn build_run_low_level(
        &mut self,
        sym: &Symbol,
        lowlevel: &LowLevel,
        args: &'a [Symbol],
        arg_layouts: &[Layout<'a>],
        ret_layout: &Layout<'a>,
    ) {
        // Now that the arguments are needed, load them if they are literals.
        self.load_literal_symbols(args);
        match lowlevel {
            LowLevel::NumAbs => {
                debug_assert_eq!(
                    1,
                    args.len(),
                    "NumAbs: expected to have exactly one argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], *ret_layout,
                    "NumAbs: expected to have the same argument and return layout"
                );
                self.build_num_abs(sym, &args[0], ret_layout)
            }
            LowLevel::NumAdd => {
                debug_assert_eq!(
                    2,
                    args.len(),
                    "NumAdd: expected to have exactly two argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], arg_layouts[1],
                    "NumAdd: expected all arguments of to have the same layout"
                );
                debug_assert_eq!(
                    arg_layouts[0], *ret_layout,
                    "NumAdd: expected to have the same argument and return layout"
                );
                self.build_num_add(sym, &args[0], &args[1], ret_layout)
            }
            LowLevel::NumAcos => self.build_fn_call(
                sym,
                bitcode::NUM_ACOS[FloatWidth::F64].to_string(),
                args,
                arg_layouts,
                ret_layout,
            ),
            LowLevel::NumAsin => self.build_fn_call(
                sym,
                bitcode::NUM_ASIN[FloatWidth::F64].to_string(),
                args,
                arg_layouts,
                ret_layout,
            ),
            LowLevel::NumAtan => self.build_fn_call(
                sym,
                bitcode::NUM_ATAN[FloatWidth::F64].to_string(),
                args,
                arg_layouts,
                ret_layout,
            ),
            LowLevel::NumMul => {
                debug_assert_eq!(
                    2,
                    args.len(),
                    "NumMul: expected to have exactly two argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], arg_layouts[1],
                    "NumMul: expected all arguments of to have the same layout"
                );
                debug_assert_eq!(
                    arg_layouts[0], *ret_layout,
                    "NumMul: expected to have the same argument and return layout"
                );
                self.build_num_mul(sym, &args[0], &args[1], ret_layout)
            }
            LowLevel::NumNeg => {
                debug_assert_eq!(
                    1,
                    args.len(),
                    "NumNeg: expected to have exactly one argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], *ret_layout,
                    "NumNeg: expected to have the same argument and return layout"
                );
                self.build_num_neg(sym, &args[0], ret_layout)
            }
            LowLevel::NumPowInt => self.build_fn_call(
                sym,
                bitcode::NUM_POW_INT[IntWidth::I64].to_string(),
                args,
                arg_layouts,
                ret_layout,
            ),
            LowLevel::NumSub => {
                debug_assert_eq!(
                    2,
                    args.len(),
                    "NumSub: expected to have exactly two argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], arg_layouts[1],
                    "NumSub: expected all arguments of to have the same layout"
                );
                debug_assert_eq!(
                    arg_layouts[0], *ret_layout,
                    "NumSub: expected to have the same argument and return layout"
                );
                self.build_num_sub(sym, &args[0], &args[1], ret_layout)
            }
            LowLevel::Eq => {
                debug_assert_eq!(2, args.len(), "Eq: expected to have exactly two argument");
                debug_assert_eq!(
                    arg_layouts[0], arg_layouts[1],
                    "Eq: expected all arguments of to have the same layout"
                );
                debug_assert_eq!(
                    Layout::Builtin(Builtin::Bool),
                    *ret_layout,
                    "Eq: expected to have return layout of type Bool"
                );
                self.build_eq(sym, &args[0], &args[1], &arg_layouts[0])
            }
            LowLevel::NotEq => {
                debug_assert_eq!(
                    2,
                    args.len(),
                    "NotEq: expected to have exactly two argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], arg_layouts[1],
                    "NotEq: expected all arguments of to have the same layout"
                );
                debug_assert_eq!(
                    Layout::Builtin(Builtin::Bool),
                    *ret_layout,
                    "NotEq: expected to have return layout of type Bool"
                );
                self.build_neq(sym, &args[0], &args[1], &arg_layouts[0])
            }
            LowLevel::NumLt => {
                debug_assert_eq!(
                    2,
                    args.len(),
                    "NumLt: expected to have exactly two argument"
                );
                debug_assert_eq!(
                    arg_layouts[0], arg_layouts[1],
                    "NumLt: expected all arguments of to have the same layout"
                );
                debug_assert_eq!(
                    Layout::Builtin(Builtin::Bool),
                    *ret_layout,
                    "NumLt: expected to have return layout of type Bool"
                );
                self.build_num_lt(sym, &args[0], &args[1], &arg_layouts[0])
            }
            LowLevel::NumToFloat => {
                debug_assert_eq!(
                    1,
                    args.len(),
                    "NumToFloat: expected to have exactly one argument"
                );
                // debug_assert_eq!(
                //     Layout::Builtin(Builtin::Float(FloatWidth::F32 | FloatWidth::F64)),
                //     *ret_layout,
                //     "NumToFloat: expected to have return layout of type Float64"
                // );
                self.build_num_to_float(sym, &args[0], &arg_layouts[0], ret_layout)
            }
            LowLevel::NumRound => self.build_fn_call(
                sym,
                bitcode::NUM_ROUND[FloatWidth::F64].to_string(),
                args,
                arg_layouts,
                ret_layout,
            ),
            LowLevel::StrConcat => self.build_fn_call(
                sym,
                bitcode::STR_CONCAT.to_string(),
                args,
                arg_layouts,
                ret_layout,
            ),
            x => unimplemented!("low level, {:?}. is not yet implemented", x),
        }
    }

    // inlines simple builtin functions that do not map directly to a low level
    fn build_inline_builtin(
        &mut self,
        sym: &Symbol,
        func_sym: Symbol,
        args: &'a [Symbol],
        arg_layouts: &[Layout<'a>],
        ret_layout: &Layout<'a>,
    ) {
        self.load_literal_symbols(args);
        match func_sym {
            Symbol::NUM_IS_ZERO => {
                debug_assert_eq!(
                    1,
                    args.len(),
                    "NumIsZero: expected to have exactly one argument"
                );
                debug_assert_eq!(
                    Layout::Builtin(Builtin::Bool),
                    *ret_layout,
                    "NumIsZero: expected to have return layout of type Bool"
                );

                self.load_literal(&Symbol::DEV_TMP, &arg_layouts[0], &Literal::Int(0));
                self.build_eq(sym, &args[0], &Symbol::DEV_TMP, &arg_layouts[0]);
                self.free_symbol(&Symbol::DEV_TMP)
            }
            _ => unimplemented!("the function, {:?}, is not yet implemented", func_sym),
        }
    }

    /// build_fn_call creates a call site for a function.
    /// This includes dealing with things like saving regs and propagating the returned value.
    fn build_fn_call(
        &mut self,
        dst: &Symbol,
        fn_name: String,
        args: &'a [Symbol],
        arg_layouts: &[Layout<'a>],
        ret_layout: &Layout<'a>,
    );

    /// build_num_abs stores the absolute value of src into dst.
    fn build_num_abs(&mut self, dst: &Symbol, src: &Symbol, layout: &Layout<'a>);

    /// build_num_add stores the sum of src1 and src2 into dst.
    fn build_num_add(&mut self, dst: &Symbol, src1: &Symbol, src2: &Symbol, layout: &Layout<'a>);

    /// build_num_mul stores `src1 * src2` into dst.
    fn build_num_mul(&mut self, dst: &Symbol, src1: &Symbol, src2: &Symbol, layout: &Layout<'a>);

    /// build_num_neg stores the negated value of src into dst.
    fn build_num_neg(&mut self, dst: &Symbol, src: &Symbol, layout: &Layout<'a>);

    /// build_num_sub stores the `src1 - src2` difference into dst.
    fn build_num_sub(&mut self, dst: &Symbol, src1: &Symbol, src2: &Symbol, layout: &Layout<'a>);

    /// build_eq stores the result of `src1 == src2` into dst.
    fn build_eq(&mut self, dst: &Symbol, src1: &Symbol, src2: &Symbol, arg_layout: &Layout<'a>);

    /// build_neq stores the result of `src1 != src2` into dst.
    fn build_neq(&mut self, dst: &Symbol, src1: &Symbol, src2: &Symbol, arg_layout: &Layout<'a>);

    /// build_num_lt stores the result of `src1 < src2` into dst.
    fn build_num_lt(&mut self, dst: &Symbol, src1: &Symbol, src2: &Symbol, arg_layout: &Layout<'a>);

    /// build_num_to_float convert Number to Float
    fn build_num_to_float(
        &mut self,
        dst: &Symbol,
        src: &Symbol,
        arg_layout: &Layout<'a>,
        ret_layout: &Layout<'a>,
    );

    /// literal_map gets the map from symbol to literal and layout, used for lazy loading and literal folding.
    fn literal_map(&mut self) -> &mut MutMap<Symbol, (&'a Literal<'a>, &'a Layout<'a>)>;

    fn load_literal_symbols(&mut self, syms: &[Symbol]) {
        if self.env().lazy_literals {
            for sym in syms {
                if let Some((lit, layout)) = self.literal_map().remove(sym) {
                    self.load_literal(sym, layout, lit);
                }
            }
        }
    }

    /// load_literal sets a symbol to be equal to a literal.
    fn load_literal(&mut self, sym: &Symbol, layout: &Layout<'a>, lit: &Literal<'a>);

    /// create_struct creates a struct with the elements specified loaded into it as data.
    fn create_struct(&mut self, sym: &Symbol, layout: &Layout<'a>, fields: &'a [Symbol]);

    /// load_struct_at_index loads into `sym` the value at `index` in `structure`.
    fn load_struct_at_index(
        &mut self,
        sym: &Symbol,
        structure: &Symbol,
        index: u64,
        field_layouts: &'a [Layout<'a>],
    );

    /// return_symbol moves a symbol to the correct return location for the backend and adds a jump to the end of the function.
    fn return_symbol(&mut self, sym: &Symbol, layout: &Layout<'a>);

    /// free_symbols will free all symbols for the given statement.
    fn free_symbols(&mut self, stmt: &Stmt<'a>) {
        if let Some(syms) = self.free_map().remove(&(stmt as *const Stmt<'a>)) {
            for sym in syms {
                // println!("Freeing symbol: {:?}", sym);
                self.free_symbol(&sym);
            }
        }
    }

    /// free_symbol frees any registers or stack space used to hold a symbol.
    fn free_symbol(&mut self, sym: &Symbol);

    /// set_last_seen sets the statement a symbol was last seen in.
    fn set_last_seen(
        &mut self,
        sym: Symbol,
        stmt: &Stmt<'a>,
        owning_symbol: &MutMap<Symbol, Symbol>,
    ) {
        self.last_seen_map().insert(sym, stmt);
        if let Some(parent) = owning_symbol.get(&sym) {
            self.last_seen_map().insert(*parent, stmt);
        }
    }

    /// last_seen_map gets the map from symbol to when it is last seen in the function.
    fn last_seen_map(&mut self) -> &mut MutMap<Symbol, *const Stmt<'a>>;

    /// set_layout_map sets the layout for a specific symbol.
    fn set_layout_map(&mut self, sym: Symbol, layout: &Layout<'a>) {
        if let Some(old_layout) = self.layout_map().insert(sym, *layout) {
            // Layout map already contains the symbol. We should never need to overwrite.
            // If the layout is not the same, that is a bug.
            if &old_layout != layout {
                internal_error!(
                    "Overwriting layout for symbol, {:?}: got {:?}, want {:?}",
                    sym,
                    layout,
                    old_layout
                )
            }
        }
    }

    /// layout_map gets the map from symbol to layout.
    fn layout_map(&mut self) -> &mut MutMap<Symbol, Layout<'a>>;

    fn create_free_map(&mut self) {
        let mut free_map = MutMap::default();
        let arena = self.env().arena;
        for (sym, stmt) in self.last_seen_map() {
            let vals = free_map
                .entry(*stmt)
                .or_insert_with(|| bumpalo::vec![in arena]);
            vals.push(*sym);
        }
        self.set_free_map(free_map);
    }

    /// free_map gets the map statement to the symbols that are free after they run.
    fn free_map(&mut self) -> &mut MutMap<*const Stmt<'a>, Vec<'a, Symbol>>;

    /// set_free_map sets the free map to the given map.
    fn set_free_map(&mut self, map: MutMap<*const Stmt<'a>, Vec<'a, Symbol>>);

    /// scan_ast runs through the ast and fill the last seen map.
    /// This must iterate through the ast in the same way that build_stmt does. i.e. then before else.
    fn scan_ast(&mut self, stmt: &Stmt<'a>) {
        // This keeps track of symbols that depend on other symbols.
        // The main case of this is data in structures and tagged unions.
        // This data must extend the lifetime of the original structure or tagged union.
        // For arrays the loading is always done through low levels and does not depend on the underlying array's lifetime.
        let mut owning_symbol: MutMap<Symbol, Symbol> = MutMap::default();
        match stmt {
            Stmt::Let(sym, expr, _, following) => {
                self.set_last_seen(*sym, stmt, &owning_symbol);
                match expr {
                    Expr::Literal(_) => {}

                    Expr::Call(call) => self.scan_ast_call(call, stmt, &owning_symbol),

                    Expr::Tag { arguments, .. } => {
                        for sym in *arguments {
                            self.set_last_seen(*sym, stmt, &owning_symbol);
                        }
                    }
                    Expr::Struct(syms) => {
                        for sym in *syms {
                            self.set_last_seen(*sym, stmt, &owning_symbol);
                        }
                    }
                    Expr::StructAtIndex { structure, .. } => {
                        self.set_last_seen(*structure, stmt, &owning_symbol);
                        owning_symbol.insert(*sym, *structure);
                    }
                    Expr::GetTagId { structure, .. } => {
                        self.set_last_seen(*structure, stmt, &owning_symbol);
                        owning_symbol.insert(*sym, *structure);
                    }
                    Expr::UnionAtIndex { structure, .. } => {
                        self.set_last_seen(*structure, stmt, &owning_symbol);
                        owning_symbol.insert(*sym, *structure);
                    }
                    Expr::Array { elems, .. } => {
                        for elem in *elems {
                            if let ListLiteralElement::Symbol(sym) = elem {
                                self.set_last_seen(*sym, stmt, &owning_symbol);
                            }
                        }
                    }
                    Expr::Reuse {
                        symbol,
                        arguments,
                        tag_name,
                        ..
                    } => {
                        self.set_last_seen(*symbol, stmt, &owning_symbol);
                        match tag_name {
                            TagName::Closure(sym) => {
                                self.set_last_seen(*sym, stmt, &owning_symbol);
                            }
                            TagName::Private(sym) => {
                                self.set_last_seen(*sym, stmt, &owning_symbol);
                            }
                            TagName::Global(_) => {}
                        }
                        for sym in *arguments {
                            self.set_last_seen(*sym, stmt, &owning_symbol);
                        }
                    }
                    Expr::Reset { symbol, .. } => {
                        self.set_last_seen(*symbol, stmt, &owning_symbol);
                    }
                    Expr::EmptyArray => {}
                    Expr::RuntimeErrorFunction(_) => {}
                }
                self.scan_ast(following);
            }

            Stmt::Switch {
                cond_symbol,
                branches,
                default_branch,
                ..
            } => {
                self.set_last_seen(*cond_symbol, stmt, &owning_symbol);
                for (_, _, branch) in *branches {
                    self.scan_ast(branch);
                }
                self.scan_ast(default_branch.1);
            }
            Stmt::Ret(sym) => {
                self.set_last_seen(*sym, stmt, &owning_symbol);
            }
            Stmt::Refcounting(modify, following) => {
                let sym = modify.get_symbol();

                self.set_last_seen(sym, stmt, &owning_symbol);
                self.scan_ast(following);
            }
            Stmt::Join {
                parameters,
                body: continuation,
                remainder,
                ..
            } => {
                for param in *parameters {
                    self.set_last_seen(param.symbol, stmt, &owning_symbol);
                }
                self.scan_ast(continuation);
                self.scan_ast(remainder);
            }
            Stmt::Jump(JoinPointId(sym), symbols) => {
                self.set_last_seen(*sym, stmt, &owning_symbol);
                for sym in *symbols {
                    self.set_last_seen(*sym, stmt, &owning_symbol);
                }
            }
            Stmt::RuntimeError(_) => {}
        }
    }

    fn scan_ast_call(
        &mut self,
        call: &roc_mono::ir::Call,
        stmt: &roc_mono::ir::Stmt<'a>,
        owning_symbol: &MutMap<Symbol, Symbol>,
    ) {
        let roc_mono::ir::Call {
            call_type,
            arguments,
        } = call;

        for sym in *arguments {
            self.set_last_seen(*sym, stmt, owning_symbol);
        }

        match call_type {
            CallType::ByName { .. } => {}
            CallType::LowLevel { .. } => {}
            CallType::HigherOrder { .. } => {}
            CallType::Foreign { .. } => {}
        }
    }
}
