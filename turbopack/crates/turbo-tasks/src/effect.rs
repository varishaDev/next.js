use std::{borrow::Cow, future::Future, panic, pin::Pin};

use anyhow::{anyhow, Result};
use auto_hash_map::AutoSet;
use parking_lot::Mutex;
use tokio::task::JoinHandle;
use tracing::{Instrument, Span};

use crate::{self as turbo_tasks, emit, manager::turbo_tasks_future_scope, CollectiblesSource, Vc};

#[turbo_tasks::value_trait]
trait Effect {}

type EffectFuture = Pin<Box<dyn Future<Output = Result<()>> + Send + Sync + 'static>>;

struct EffectInner {
    future: EffectFuture,
    span: Span,
}

#[turbo_tasks::value(serialization = "none", cell = "new", eq = "manual")]
struct EffectInstance {
    #[turbo_tasks(trace_ignore, debug_ignore)]
    inner: Mutex<Option<EffectInner>>,
}

impl EffectInstance {
    fn new(future: impl Future<Output = Result<()>> + Send + Sync + 'static) -> Self {
        Self {
            inner: Mutex::new(Some(EffectInner {
                future: Box::pin(future),
                span: Span::current(),
            })),
        }
    }

    pub fn apply(&self) -> Option<JoinHandle<Result<()>>> {
        let future = self.inner.lock().take();
        future.map(|EffectInner { future, span }| {
            tokio::spawn(
                turbo_tasks_future_scope(turbo_tasks::turbo_tasks(), async move { future.await })
                    .instrument(span),
            )
        })
    }
}

#[turbo_tasks::value_impl]
impl Effect for EffectInstance {}

pub fn effect(future: impl Future<Output = Result<()>> + Send + Sync + 'static) {
    emit::<Box<dyn Effect>>(Vc::upcast(EffectInstance::new(future).cell()));
}

pub async fn apply_effects(source: impl CollectiblesSource) -> Result<()> {
    let effects: AutoSet<Vc<Box<dyn Effect>>> = source.take_collectibles();
    if effects.is_empty() {
        return Ok(());
    }
    let span = tracing::span!(tracing::Level::INFO, "apply effects", count = effects.len());
    async move {
        let mut first_error = anyhow::Ok(());
        for effect in effects {
            let Some(effect) = Vc::try_resolve_downcast_type::<EffectInstance>(effect).await?
            else {
                panic!("Effect must only be implemented by EffectInstance");
            };
            if let Some(join_handle) = effect.await?.apply() {
                match join_handle.await {
                    Ok(Err(err)) if first_error.is_ok() => {
                        first_error = Err(err);
                    }
                    Err(err) if first_error.is_ok() => {
                        let any = err.into_panic();
                        let panic = match any.downcast::<String>() {
                            Ok(owned) => Some(Cow::Owned(*owned)),
                            Err(any) => match any.downcast::<&'static str>() {
                                Ok(str) => Some(Cow::Borrowed(*str)),
                                Err(_) => None,
                            },
                        };
                        first_error = Err(if let Some(panic) = panic {
                            anyhow!("Task effect panicked: {panic}")
                        } else {
                            anyhow!("Task effect panicked")
                        });
                    }
                    _ => {}
                }
            }
        }
        first_error
    }
    .instrument(span)
    .await
}

#[cfg(test)]
mod tests {
    use crate::{apply_effects, CollectiblesSource};

    #[test]
    #[allow(dead_code)]
    fn apply_effects_is_sync_and_send() {
        fn assert_sync<T: Sync + Send>(_: T) {}
        fn check<T: CollectiblesSource + Send + Sync>(t: T) {
            assert_sync(apply_effects(t));
        }
    }
}
