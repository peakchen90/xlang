use crate::helper::never;
use crate::scope::{BlockScope, ScopeType};
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::execution_engine::ExecutionEngine;
use inkwell::module::Module;
use inkwell::types::{BasicMetadataTypeEnum, FloatType, FunctionType};
use inkwell::values::{
    BasicMetadataValueEnum, BasicValue, BasicValueEnum, CallableValue,
    FunctionValue, PointerValue,
};
use std::ops::Deref;
use inkwell::basic_block::BasicBlock;
use x_lang_ast::shared::{Kind, KindName, Node};
use x_lang_ast::visitor::Visitor;

pub struct Compiler<'a, 'ctx> {
    pub context: &'ctx Context,
    pub builder: &'a Builder<'ctx>,
    pub module: &'a Module<'ctx>,
    pub scope: &'a mut BlockScope<'ctx>,
    // pub execution_engine: ExecutionEngine<'ctx>,
}

impl<'a, 'ctx> Compiler<'a, 'ctx> {
    pub fn compile(ast: &Node) {
        let context = &Context::create();
        let module = &context.create_module("main");
        let builder = &context.create_builder();
        let scope = &mut BlockScope::new();

        let mut compiler = Compiler {
            context,
            module,
            builder,
            scope,
        };

        compiler.compile_program(ast);

        println!(" ========================= PRINT IR ============================\n");

        // 控制台打印 IR 码
        compiler.module.print_to_stderr();
    }

    fn put_variable(
        &mut self,
        name: &str,
        kind: Kind,
        value: Option<BasicValueEnum<'ctx>>,
        is_arg: bool, // 是否是函数参数
    ) {
        let mut current = self.scope.current().unwrap();
        if current.has(name) {
            panic!("Scope name `{}` is exist", name);
        }

        // 内存中的描述名称
        let mem_name = match is_arg {
            true => format!("argument.{}", name),
            false => name.to_string(),
        };
        let ptr = match kind.read_kind_name().unwrap() {
            KindName::Number => {
                let ty = self.build_number_type();
                let ptr = self.builder.build_alloca(ty, &mem_name);
                if let Some(v) = value {
                    self.builder.build_store(ptr, v.into_float_value());
                }
                Some(ptr)
            }
            KindName::Void => None,
        };

        let scope_type = ScopeType::Variable { kind, ptr };
        self.scope.put_variable(name, scope_type);
    }

    fn put_function(
        &mut self,
        name: &str,
        fn_value: &FunctionValue<'ctx>,
        arg_kind_names: Vec<KindName>,
        return_kind: Kind,
    ) {
        let mut current = self.scope.current().unwrap();
        if current.has(name) {
            panic!("Scope name `{}` is exist", name);
        }

        let scope_type = ScopeType::Function {
            fn_value: *fn_value,
            return_kind,
            arg_kind_names,
        };
        self.scope.put_variable(name, scope_type);
    }

    fn push_block_scope(&mut self, basic_block: &BasicBlock<'ctx>) {
        self.scope.push(basic_block);
        self.builder.position_at_end(*basic_block);
    }

    fn pop_block_scope(&mut self) {
        self.scope.pop();
        if let Some(v) = self.scope.current() {
            self.builder.position_at_end(v.basic_block);
        }
    }

    // 推断表达式的返回类型名称
    fn infer_expression_kind(&self, expr: &Node) -> Kind {
        let mut ret_kind = Kind::None;

        Visitor::walk(expr, &mut |node, mut visitor| match node {
            Node::CallExpression { callee, .. } => {
                let (id, ..) = callee.deref().read_identifier();
                match self.scope.search_by_name(id) {
                    Some(v) => {
                        if v.is_fn() {
                            let (.., return_kind) = v.get_fn();
                            ret_kind = *return_kind;
                            visitor.stop();
                        }
                    }
                    _ => {}
                }
            }
            Node::BinaryExpression { left, .. } => {
                ret_kind = self.infer_expression_kind(left.deref());
                visitor.stop();
            }
            Node::AssignmentExpression { left, .. } => {
                ret_kind = self.infer_expression_kind(left.deref());
                visitor.stop();
            }
            Node::Identifier { name, kind } => match kind {
                Kind::Some(_) => {
                    ret_kind = *kind;
                    visitor.stop();
                }
                Kind::Infer => match self.scope.search_by_name(name) {
                    Some(v) => {
                        if v.is_var() {
                            if kind.is_exact() {
                                let (.., return_kind) = v.get_fn();
                                ret_kind = *return_kind;
                                visitor.stop();
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            Node::NumberLiteral { .. } => {
                ret_kind = Kind::Some(KindName::Number);
                visitor.stop();
            }
            _ => {}
        });
        ret_kind
    }

    fn compile_program(&mut self, node: &Node) {
        match node {
            Node::Program { body } => {
                self.compile_function(
                    "entry",
                    &vec![],
                    &vec![],
                    body,
                    &Kind::Some(KindName::Void),
                );
            }
            _ => {}
        }
    }

    fn compile_statement(&mut self, node: &Node) {
        match node {
            Node::FunctionDeclaration {
                id,
                arguments,
                body,
                return_kind,
            } => {
                let (name, ..) = id.deref().read_identifier();
                let args = arguments.iter();
                let args = args.map(|arg| {
                    let (.., kind) = arg.deref().read_identifier();
                    let kind_name = kind.read_kind_name().unwrap();
                    (match kind_name {
                        KindName::Number => self.build_number_type(),
                        KindName::Void => never(),
                    })
                    .into()
                });
                let args = args.collect();

                let body = match body.deref() {
                    Node::BlockStatement { body } => body,
                    _ => never(),
                };
                self.compile_function(
                    name,
                    &args,
                    arguments,
                    body,
                    return_kind,
                );
            }
            Node::VariableDeclaration { id, init } => {
                let (id, mut kind) = id.deref().read_identifier();
                let init = init.deref();

                // Note: 避免下面的临时变量生命周期不够长，临时借用变量
                let temp_borrowed;
                if !kind.is_exact() {
                    temp_borrowed = self.infer_expression_kind(init);
                    kind = &temp_borrowed;
                }
                let init_value = self.compile_expression(init);
                self.put_variable(
                    id,
                    *kind,
                    Some(init_value.as_basic_value_enum()),
                    false
                );
            }
            Node::BlockStatement { body } => {
                self.compile_block_statement(body, false);
            }
            Node::ReturnStatement { argument } => {
                self.compile_expression(argument.deref());
            }
            Node::ExpressionStatement { expression } => {
                self.compile_expression(expression.deref());
            }
            _ => {}
        }
    }

    /// 编译函数声明
    fn compile_function(
        &mut self,
        name: &str,
        args: &Vec<BasicMetadataTypeEnum<'ctx>>,
        arguments: &Vec<Box<Node>>,
        body: &Vec<Box<Node>>,
        return_kind: &Kind,
    ) {
        let fn_type = match return_kind.read_kind_name().unwrap() {
            KindName::Number => {
                self.context.f64_type().fn_type(args.as_slice(), false)
            }
            KindName::Void => {
                self.context.void_type().fn_type(args.as_slice(), false)
            }
        };

        let fn_value = self.module.add_function(name, fn_type, None);

        // 为每个参数设置名称
        for (i, arg) in fn_value.get_param_iter().enumerate() {
            let (arg_name, kind) = arguments[i].deref().read_identifier();
            match kind.read_kind_name().unwrap() {
                KindName::Number => {
                    let fv = arg.into_float_value();
                    fv.set_name(arg_name);
                }
                KindName::Void => {}
            }
        }

        let block = self.context.append_basic_block(fn_value, "");
        self.push_block_scope(&block); // 作用域入栈

        // 设置形参
        for (i, arg) in fn_value.get_param_iter().enumerate() {
            let (arg_name, kind) = arguments[i].deref().read_identifier();
            let arg_value = fn_value.get_nth_param(i as u32).unwrap();
            self.put_variable(arg_name, *kind, Some(arg_value), true);
        }

        // compile function body
        self.compile_block_statement(body, true);

        self.pop_block_scope();
    }

    fn compile_block_statement(
        &mut self,
        node: &Vec<Box<Node>>,
        is_fn_block: bool,
    ) {
        if !is_fn_block {
            // TODO: 暂不实现块作用域
            self.compile_function(
                "anonymous",
                &vec![],
                &vec![],
                node,
                &Kind::Some(KindName::Void),
            );
        } else {
            for stat in node {
                self.compile_statement(stat.deref());
            }
        }
    }

    fn compile_expression(&self, node: &Node) -> BasicValueEnum<'ctx> {
        match node {
            Node::CallExpression { callee, arguments } => {
                let (name, ..) = callee.deref().read_identifier();
                let fn_value = self.get_declare_fn_val(name);
                let args = arguments.iter();
                let args = args.map(|arg| {
                    let value = self.compile_expression(arg.deref());
                    BasicMetadataValueEnum::from(value)
                });
                // let args = args.into_iter();
                let args = args.collect::<Vec<BasicMetadataValueEnum>>();
                let args = args.as_slice();

                self.builder
                    .build_call(*fn_value, args, &format!("call_{}", name))
                    .try_as_basic_value()
                    .left()
                    .unwrap()
            }
            Node::BinaryExpression {
                left,
                right,
                operator,
            } => {
                let left = self.compile_expression(left.deref());
                let right = self.compile_expression(right.deref());
                self.compile_binary_expression(&left, &right, operator)
            }
            Node::AssignmentExpression {
                left,
                right,
                operator,
            } => {
                let (left_var, ..) = left.deref().read_identifier();
                let ptr = self.get_declare_var_ptr(left_var);
                let right = self.compile_expression(right.deref());
                match ptr {
                    Some(ptr) => self.builder.build_store(*ptr, right),
                    None => panic!("Can not assign a value on void type"),
                };
                right
            }
            Node::Identifier { name, .. } => {
                let ptr = self.get_declare_var_ptr(name);
                match ptr {
                    Some(ptr) => self.builder.build_load(*ptr, name),
                    None => panic!("Can not get value on void type"),
                }
            }
            Node::NumberLiteral { value } => {
                self.build_number_value(*value).as_basic_value_enum()
            }
            _ => never(),
        }
    }

    fn compile_binary_expression(
        &self,
        left: &BasicValueEnum<'ctx>,
        right: &BasicValueEnum<'ctx>,
        operator: &str,
    ) -> BasicValueEnum<'ctx> {
        if operator == "+" {
            self.builder
                .build_float_add(
                    left.into_float_value(),
                    right.into_float_value(),
                    "add",
                )
                .as_basic_value_enum()
        } else if operator == "-" {
            self.builder
                .build_float_sub(
                    left.into_float_value(),
                    right.into_float_value(),
                    "sub",
                )
                .as_basic_value_enum()
        } else if operator == "*" {
            self.builder
                .build_float_mul(
                    left.into_float_value(),
                    right.into_float_value(),
                    "mul",
                )
                .as_basic_value_enum()
        } else if operator == "/" {
            self.builder
                .build_float_div(
                    left.into_float_value(),
                    right.into_float_value(),
                    "div",
                )
                .as_basic_value_enum()
        } else {
            never()
        }
    }
}
