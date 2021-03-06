use crate::{Scripts, PROJECT_DIRS};
use dirty2::Dirty;
use rusty_v8 as v8;
use simple_error::SimpleError;
use std::{cell::RefCell, convert::TryFrom, fs::File, io::Read, rc::Rc};

static BOOP_WRAPPER_START: &str = "
/***********************************
*     Start of Boop's wrapper      *
***********************************/
            
(function() {
    var module = {
        exports: {}
    };
            
    const moduleWrapper = (function (exports, module) {

/***********************************
*      End of Boop's wrapper      *
***********************************/

";

static BOOP_WRAPPER_END: &str = "
            
/***********************************
*     Start of Boop's wrapper      *
***********************************/
            
    }).apply(module.exports, [module.exports, module]);

    return module.exports;
})();
            
/***********************************
*      End of Boop's wrapper      *
***********************************/
";

pub struct Executor {
    isolate: v8::OwnedIsolate,
}

struct ExecutorState {
    global_context: Option<v8::Global<v8::Context>>,
    main_function: Option<v8::Global<v8::Function>>,
}

#[derive(Clone, Debug, Default)]
pub struct ExecutionStatus {
    // true if text was selected when execution began
    is_text_selected: bool,

    info: Option<String>,
    error: Option<String>,

    insert: Vec<String>,
    full_text: Dirty<String>,
    text: Dirty<String>,
    selection: Dirty<String>,
}

impl ExecutionStatus {
    fn reset(&mut self) {
        self.info = None;
        self.error = None;
        self.insert.clear();
        self.full_text.write().clear();
        Dirty::clear(&mut self.full_text);
        self.text.write().clear();
        Dirty::clear(&mut self.text);
    }

    pub fn info(&self) -> Option<&String> {
        self.info.as_ref()
    }

    pub fn error(&self) -> Option<&String> {
        self.error.as_ref()
    }

    pub fn into_replacement(self) -> TextReplacement {
        // not quite sure what the correct behaviour here should be
        // right now the order of presidence is:
        // 0. insertion
        // 1. fullText
        // 2. selection
        // 3. text (with select)
        // 4. text (without selection)
        // TODO: move into ExecutionStatus
        if !self.insert.is_empty() {
            info!("found insertion");
            TextReplacement::Insert(self.insert)
        } else if self.full_text.dirty() {
            info!("found full_text replacement");
            TextReplacement::Full(self.full_text.unwrap())
        } else if self.selection.dirty() {
            info!("found selection replacement");
            TextReplacement::Selection(self.selection.unwrap())
        } else if self.is_text_selected && self.text.dirty() {
            info!("found text (with selection) replacement");
            TextReplacement::Selection(self.text.unwrap())
        } else if self.text.dirty() {
            info!("found text (without selection) replacement");
            TextReplacement::Full(self.text.unwrap())
        } else {
            TextReplacement::None
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum TextReplacement {
    Full(String),
    Selection(String),
    Insert(Vec<String>),
    None,
}

impl Executor {
    pub fn new(source: &str) -> Self {
        info!("initalizing isolate");

        // set up execution context
        let mut isolate = v8::Isolate::new(Default::default());
        let (global_context, main_function) = {
            let scope = &mut v8::HandleScope::new(&mut isolate);
            // let context = v8::Context::new(scope);
            let (context, main_function) = Executor::initialize_context(source, scope);
            (v8::Global::new(scope, context), main_function)
        };

        // set status slot, stores execution infomation
        let status_slot: Rc<RefCell<ExecutionStatus>> =
            Rc::new(RefCell::new(ExecutionStatus::default()));
        isolate.set_slot(status_slot);

        // set state slot, stores v8 details
        let state_slot: Rc<RefCell<ExecutorState>> = Rc::new(RefCell::new(ExecutorState {
            global_context: Some(global_context),
            main_function: Some(main_function),
        }));
        isolate.set_slot(state_slot);

        Executor { isolate }
    }

    // load source code from internal files or external filesystem depending on the path
    fn load_raw_source(path: String) -> Result<String, SimpleError> {
        if path.starts_with("@boop/") {
            // script is internal

            let internal_path = path.replace("@boop/", "lib/");
            info!(
                "found internal script, real path: #BINARY#/{}",
                internal_path
            );

            let raw_source = String::from_utf8(
                Scripts::get(&internal_path)
                    .ok_or_else(|| {
                        SimpleError::new(format!("no internal script with path \"{}\"", path))
                    })?
                    .to_vec(),
            )
            .map_err(|e| SimpleError::with("problem with file encoding", e))?;

            return Ok(raw_source);
        }

        let mut external_path = PROJECT_DIRS.config_dir().to_path_buf();
        external_path.push("scripts");
        external_path.push(&path);

        info!(
            "found external script, real path: {}",
            external_path.display()
        );

        let mut raw_source = String::new();
        File::open(external_path)
            .map_err(|e| SimpleError::with(&format!("could not open \"{}\"", path), e))?
            .read_to_string(&mut raw_source)
            .map_err(|e| SimpleError::with("problem reading file", e))?;

        Ok(raw_source)
    }

    fn initialize_context<'s>(
        source: &str,
        scope: &mut v8::HandleScope<'s, ()>,
    ) -> (v8::Local<'s, v8::Context>, v8::Global<v8::Function>) {
        let scope = &mut v8::EscapableHandleScope::new(scope);
        let context = v8::Context::new(scope);
        let global = context.global(scope);
        let scope = &mut v8::ContextScope::new(scope, context);

        let require_key =
            v8::String::new(scope, "require").expect("failed to created 'require' string");
        let require_val = v8::Function::new(scope, Executor::global_require)
            .expect("failed to created require function");
        global.set(scope, require_key.into(), require_val.into());

        // complile and run script
        let code = v8::String::new(scope, source).expect("failed to created JS string");
        let compiled_script =
            v8::Script::compile(scope, code, None).expect("failed to compile script");

        let tc_scope = &mut v8::TryCatch::new(scope);
        let result = compiled_script.run(tc_scope);

        if result.is_none() {
            assert!(tc_scope.has_caught());
            let exception = tc_scope
                .exception()
                .expect("exception was caught, but exception is none");

            error!(
                "<<JS EXCEPTION>> {}",
                exception
                    .to_string(tc_scope)
                    .expect("failed to convert exception to string")
                    .to_rust_string_lossy(tc_scope),
            );
        }

        // extract main function
        let main_key =
            v8::String::new(tc_scope, "main").expect("failed to create JS string 'main'");
        let main_function =
            v8::Local::<v8::Function>::try_from(global.get(tc_scope, main_key.into()).unwrap())
                .expect("failed to get main function");
        let main_function = v8::Global::new(tc_scope, main_function);

        (tc_scope.escape(context), main_function)
    }

    pub fn execute(&mut self, full_text: &str, selection: Option<&str>) -> ExecutionStatus {
        // setup execution status
        {
            let status_slot = self
                .isolate
                .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
                .expect("failed to get mutable access to status slot");

            let mut status = status_slot.borrow_mut();

            status.reset();
            *status.full_text.write() = full_text.to_string();
            status.full_text.clear();
            *status.text.write() = selection.unwrap_or(full_text).to_string();
            status.text.clear();
            *status.selection.write() = selection.unwrap_or("").to_string();
            status.selection.clear();
        }

        // prepare payload and execute main
        // TODO: use ObjectTemplate, problem: rusty_v8 doesn't have set_accessor_with_setter or even set_accessor for
        // object templates
        {
            let state_slot = self
                .isolate
                .get_slot_mut::<Rc<RefCell<ExecutorState>>>()
                .expect("failed to get mutable access to state slot")
                .clone();
            let state_slot = state_slot.borrow();

            let context = state_slot
                .global_context
                .as_ref()
                .expect("global_context is not initalizied");
            let scope = &mut v8::HandleScope::with_context(&mut self.isolate, context);

            // payload is the object passed into function main
            let payload = v8::Object::new(scope);

            // getter/setters: full_text, text, selection
            {
                let full_text_key = v8::String::new(scope, "fullText")
                    .expect("failed to create JS string for 'fullText'");
                let text_key =
                    v8::String::new(scope, "text").expect("failed to create JS string for 'text'");
                let selection_key = v8::String::new(scope, "selection")
                    .expect("failed to create JS string for 'selection'");

                payload.set_accessor_with_setter(
                    scope,
                    full_text_key.into(),
                    Executor::payload_full_text_getter,
                    Executor::payload_full_text_setter,
                );
                payload.set_accessor_with_setter(
                    scope,
                    text_key.into(),
                    Executor::payload_text_getter,
                    Executor::payload_text_setter,
                );
                payload.set_accessor_with_setter(
                    scope,
                    selection_key.into(),
                    Executor::payload_selection_getter,
                    Executor::payload_selection_setter,
                );
            }

            // functions: post_info, post_error, insert
            {
                let post_info_key = v8::String::new(scope, "postInfo")
                    .expect("failed to create JS string 'postInfo'");
                let post_error_key = v8::String::new(scope, "postError")
                    .expect("failed to create JS string 'postError'");
                let insert_key =
                    v8::String::new(scope, "insert").expect("failed to create JS string 'insert'");

                let post_info_val = v8::Function::new(scope, Executor::payload_post_info)
                    .expect("failed to convert post_info function");
                let post_error_val = v8::Function::new(scope, Executor::payload_post_error)
                    .expect("failed to create post_error function");
                let insert_val = v8::Function::new(scope, Executor::payload_insert)
                    .expect("failed to create payload_insert function");

                payload.set(scope, post_info_key.into(), post_info_val.into());
                payload.set(scope, post_error_key.into(), post_error_val.into());
                payload.set(scope, insert_key.into(), insert_val.into());
            }

            let main_function = state_slot
                .main_function
                .as_ref()
                .expect("main_function not initialized")
                .get(scope);
            let tc_scope = &mut v8::TryCatch::new(scope);
            let result = main_function.call(tc_scope, payload.into(), &[payload.into()]);

            if result.is_none() {
                assert!(tc_scope.has_caught());
                let exception = tc_scope
                    .exception()
                    .expect("failed to get exception, but exception was caught");

                error!(
                    "<<JS EXCEPTION>> {}",
                    exception
                        .to_string(tc_scope)
                        .expect("failed to convert exception to string")
                        .to_rust_string_lossy(tc_scope),
                );
            }
        }

        // extract execution status
        {
            let status_slot = self
                .isolate
                .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
                .expect("failed to get mutable access to status slot");

            let status = (status_slot).borrow();

            status.clone()
        }
    }

    fn global_require(
        scope: &mut v8::HandleScope<'_>,
        args: v8::FunctionCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let mut path = args
            .get(0)
            .to_string(scope)
            .expect("failed to convert argument to require to string")
            .to_rust_string_lossy(scope);

        info!("loading {}", path);

        // append extension
        if !path.ends_with(".js") {
            path.push_str(".js");
        }

        match Executor::load_raw_source(path) {
            Ok(raw_source) => {
                let source = format!("{}{}{}", BOOP_WRAPPER_START, raw_source, BOOP_WRAPPER_END);

                let code = v8::String::new(scope, &source)
                    .expect("failed to create JS string from source");
                let compiled_script =
                    v8::Script::compile(scope, code, None).expect("failed to compile script");

                let tc_scope = &mut v8::TryCatch::new(scope);
                let export = compiled_script.run(tc_scope);

                match export {
                    Some(export) => rv.set(export),
                    None => {
                        assert!(tc_scope.has_caught());
                        let exception = tc_scope
                            .exception()
                            .expect("failed to get exception, but exception was caught");

                        error!(
                            "<<JS EXCEPTION>> {}",
                            exception
                                .to_string(tc_scope)
                                .expect("failed to convert exception to string")
                                .to_rust_string_lossy(tc_scope),
                        );
                    }
                }
            }
            Err(e) => {
                warn!("problem requiring script, {}", e);

                let undefined = v8::undefined(scope).into();
                rv.set(undefined)
            }
        }
    }

    fn payload_post_info(
        scope: &mut v8::HandleScope<'_>,
        args: v8::FunctionCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let info = args
            .get(0)
            .to_string(scope)
            .expect("failed to convert argument to post_info to string")
            .to_rust_string_lossy(scope);

        scope
            .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get mutable access to status slot")
            .borrow_mut()
            .info
            .replace(info);

        let undefined = v8::undefined(scope).into();
        rv.set(undefined)
    }

    fn payload_post_error(
        scope: &mut v8::HandleScope<'_>,
        args: v8::FunctionCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let error = args
            .get(0)
            .to_string(scope)
            .expect("failed to convert argument to post_error to string")
            .to_rust_string_lossy(scope);

        scope
            .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get mutable access to status slot")
            .borrow_mut()
            .error
            .replace(error);

        let undefined = v8::undefined(scope).into();
        rv.set(undefined)
    }

    fn payload_insert(
        scope: &mut v8::HandleScope<'_>,
        args: v8::FunctionCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let insert = args
            .get(0)
            .to_string(scope)
            .expect("failed to convert insert argument to string")
            .to_rust_string_lossy(scope);

        scope
            .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get mutable access to status slot")
            .borrow_mut()
            .insert
            .push(insert);

        let undefined = v8::undefined(scope).into();
        rv.set(undefined)
    }

    fn payload_full_text_getter(
        scope: &mut v8::HandleScope<'_>,
        _key: v8::Local<'_, v8::Name>,
        _args: v8::PropertyCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let full_text = scope
            .get_slot::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get status slot")
            .borrow()
            .full_text
            .read()
            .clone();

        rv.set(
            v8::String::new(scope, &full_text)
                .expect("failed to construct JS string from full_text")
                .into(),
        );
    }

    fn payload_full_text_setter(
        scope: &mut v8::HandleScope<'_>,
        _key: v8::Local<'_, v8::Name>,
        value: v8::Local<'_, v8::Value>,
        _args: v8::PropertyCallbackArguments<'_>,
    ) {
        let new_value = value
            .to_string(scope)
            .expect("failed to convert value to string")
            .to_rust_string_lossy(scope);

        info!("setting full_text ({} bytes)", new_value.len());

        let slot = scope
            .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get mutable access to status slot");

        let mut slot = slot.borrow_mut();

        let full_text = slot.full_text.write();

        *full_text = new_value;
    }

    fn payload_text_getter(
        scope: &mut v8::HandleScope<'_>,
        _key: v8::Local<'_, v8::Name>,
        _args: v8::PropertyCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let text = scope
            .get_slot::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get status slot")
            .borrow()
            .text
            .read()
            .clone();

        rv.set(
            v8::String::new(scope, &text)
                .expect("faield to create JS string from text")
                .into(),
        );
    }

    fn payload_text_setter(
        scope: &mut v8::HandleScope<'_>,
        _key: v8::Local<'_, v8::Name>,
        value: v8::Local<'_, v8::Value>,
        _args: v8::PropertyCallbackArguments<'_>,
    ) {
        let new_value = value
            .to_string(scope)
            .expect("failed to convert value to string")
            .to_rust_string_lossy(scope);

        info!("setting text ({} bytes)", new_value.len());

        let slot = scope
            .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
            .expect("faield to get mutable access status slot");

        let mut slot = slot.borrow_mut();

        let text = slot.text.write();

        *text = new_value;
    }

    fn payload_selection_getter(
        scope: &mut v8::HandleScope<'_>,
        _key: v8::Local<'_, v8::Name>,
        _args: v8::PropertyCallbackArguments<'_>,
        mut rv: v8::ReturnValue<'_>,
    ) {
        let selection = scope
            .get_slot::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get status slot")
            .borrow()
            .selection
            .read()
            .clone();

        rv.set(
            v8::String::new(scope, &selection)
                .expect("problem constructing JS string")
                .into(),
        );
    }

    fn payload_selection_setter(
        scope: &mut v8::HandleScope<'_>,
        _key: v8::Local<'_, v8::Name>,
        value: v8::Local<'_, v8::Value>,
        _args: v8::PropertyCallbackArguments<'_>,
    ) {
        let new_value = value
            .to_string(scope)
            .expect("failed to convert value to string")
            .to_rust_string_lossy(scope);

        info!("setting selection ({} bytes)", new_value.len());

        let slot = scope
            .get_slot_mut::<Rc<RefCell<ExecutionStatus>>>()
            .expect("failed to get mutable access to status slot");

        let mut slot = slot.borrow_mut();

        let selection = slot.selection.write();

        *selection = new_value;
    }
}
