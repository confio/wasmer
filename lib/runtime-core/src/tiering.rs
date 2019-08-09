use crate::backend::{Compiler, CompilerConfig};
use crate::import::ImportObject;
use crate::fault::{
    catch_unsafe_unwind, ensure_sighandler, with_ctx, push_code_version, pop_code_version,
};
use crate::state::{
    x64::invoke_call_return_on_stack, InstanceImage, CodeVersion
};
use crate::vm::Ctx;
use crate::compile_with_config;
use crate::instance::Instance;
use crate::fault::{set_wasm_interrupt_on_ctx, was_sigint_triggered_fault};
use crate::module::{Module, ModuleInfo};

use std::sync::{Arc, Mutex};
use std::cell::Cell;

struct Defer<F: FnOnce()>(Option<F>);
impl<F: FnOnce()> Drop for Defer<F> {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

pub enum ShellExitOperation {
    ContinueWith(InstanceImage),
}

pub struct InteractiveShellContext {
    pub image: Option<InstanceImage>,
}

struct OptimizationState {
    outcome: Mutex<Option<OptimizationOutcome>>,
}

struct OptimizationOutcome {
    module: Module,
}

#[repr(transparent)]
struct CtxWrapper(*mut Ctx);
unsafe impl Send for CtxWrapper {}
unsafe impl Sync for CtxWrapper {}

unsafe fn do_optimize(
    binary: &[u8],
    compiler: Box<dyn Compiler>,
    ctx: &Mutex<CtxWrapper>,
    state: &OptimizationState,
) {
    let module = match compile_with_config(
        &binary[..],
        &*compiler,
        CompilerConfig {
            symbol_map: None,
            track_state: true,
            ..Default::default()
        },
    ) {
        Ok(x) => x,
        Err(_) => return,
    };

    let ctx_inner = ctx.lock().unwrap();
    if !ctx_inner.0.is_null() {
        *state.outcome.lock().unwrap() = Some(OptimizationOutcome { module });
        set_wasm_interrupt_on_ctx(ctx_inner.0);
    }
}

pub fn run_tiering<F: Fn(InteractiveShellContext) -> ShellExitOperation>(
    module_info: &ModuleInfo,
    wasm_binary: &[u8],
    mut resume_image: Option<InstanceImage>,
    import_object: &ImportObject,
    start_raw: extern "C" fn(&mut Ctx),
    baseline: &mut Instance,
    optimized_backends: Vec<Box<Fn() -> Box<dyn Compiler> + Send>>,
    interactive_shell: F,
) -> Result<(), String> {
    unsafe {
        ensure_sighandler();

        let ctx_box =
            Arc::new(Mutex::new(CtxWrapper(baseline.context_mut() as *mut _)));
        // Ensure that the ctx pointer's lifetime is not longer than Instance's.
        let _deferred_ctx_box_cleanup: Defer<_> = {
            let ctx_box = ctx_box.clone();
            Defer(Some(move || {
                ctx_box.lock().unwrap().0 = ::std::ptr::null_mut();
            }))
        };
        let opt_state = Arc::new(OptimizationState {
            outcome: Mutex::new(None),
        });

        {
            let wasm_binary = wasm_binary.to_vec();
            let ctx_box = ctx_box.clone();
            let opt_state = opt_state.clone();
            ::std::thread::spawn(move || {
                for backend in optimized_backends {
                    if !ctx_box.lock().unwrap().0.is_null() {
                        do_optimize(
                            &wasm_binary,
                            backend(),
                            &ctx_box,
                            &opt_state,
                        );
                    }
                }
            });
        }

        let mut optimized_instances: Vec<Instance> = vec![];

        push_code_version(CodeVersion {
            msm: baseline.module.runnable_module.get_module_state_map().unwrap(),
            base: baseline.module.runnable_module.get_code().unwrap().as_ptr() as usize,
        });
        let n_versions: Cell<usize> = Cell::new(1);

        let _deferred_pop_versions = Defer(Some(|| {
            for _ in 0..n_versions.get() {
                pop_code_version().unwrap();
            }
        }));

        loop {
            let new_optimized: Option<&mut Instance> = {
                let mut outcome = opt_state.outcome.lock().unwrap();
                if let Some(x) = outcome.take() {
                    let instance =
                        x.module.instantiate(&import_object).map_err(|e| {
                            format!("Can't instantiate module: {:?}", e)
                        })?;
                    // Keep the optimized code alive.
                    optimized_instances.push(instance);
                    optimized_instances.last_mut()
                } else {
                    None
                }
            };
            if let Some(optimized) = new_optimized {
                let base = module_info.imported_functions.len();
                let code_ptr = optimized
                    .module
                    .runnable_module
                    .get_code()
                    .unwrap()
                    .as_ptr()
                    as usize;
                let target_addresses: Vec<usize> = optimized
                    .module
                    .runnable_module
                    .get_local_function_offsets()
                    .unwrap()
                    .into_iter()
                    .map(|x| code_ptr + x)
                    .collect();
                assert_eq!(
                    target_addresses.len(),
                    module_info.func_assoc.len() - base
                );
                for i in base..module_info.func_assoc.len() {
                    baseline
                        .module
                        .runnable_module
                        .patch_local_function(i - base, target_addresses[i - base]);
                }

                push_code_version(CodeVersion {
                    msm: optimized.module.runnable_module.get_module_state_map().unwrap(),
                    base: optimized.module.runnable_module.get_code().unwrap().as_ptr() as usize,
                });
                n_versions.set(n_versions.get() + 1);

                eprintln!("Patched");
            }
            // TODO: Fix this for optimized version.
            let breakpoints = baseline.module.runnable_module.get_breakpoints();
            let ctx = baseline.context_mut() as *mut _;
            let ret = with_ctx(ctx, || {
                if let Some(image) = resume_image.take() {
                    let msm = baseline
                        .module
                        .runnable_module
                        .get_module_state_map()
                        .unwrap();
                    let code_base =
                        baseline.module.runnable_module.get_code().unwrap().as_ptr()
                            as usize;
                    invoke_call_return_on_stack(
                        &msm,
                        code_base,
                        image,
                        baseline.context_mut(),
                        breakpoints.clone(),
                    )
                    .map(|_| ())
                } else {
                    catch_unsafe_unwind(
                        || start_raw(baseline.context_mut()),
                        breakpoints.clone(),
                    )
                }
            });
            if let Err(e) = ret {
                if let Some(new_image) = e.downcast_ref::<InstanceImage>() {
                    // Tier switch event
                    if !was_sigint_triggered_fault()
                        && opt_state.outcome.lock().unwrap().is_some()
                    {
                        resume_image = Some(new_image.clone());
                        continue;
                    }
                    let op = interactive_shell(InteractiveShellContext {
                        image: Some(new_image.clone()),
                    });
                    match op {
                        ShellExitOperation::ContinueWith(new_image) => {
                            resume_image = Some(new_image);
                        }
                    }
                } else {
                    return Err("Error while executing WebAssembly".into());
                }
            } else {
                return Ok(());
            }
        }
    }
}