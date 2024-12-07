use super::{use_bundle_inner, RuntimeContext, SpawnContext, SystemParamFunction};
use crate::{
    compose::Compose, data::Data, use_context, use_drop, use_provider, use_ref, Scope, Signal,
};
use bevy_ecs::{entity::Entity, prelude::*, world::World};
use bevy_hierarchy::BuildChildren;
use std::{
    cell::Cell,
    mem,
    sync::{Arc, Mutex},
};

/// Create a [`Spawn`] composable that spawns the provided `bundle` when composed.
///
/// On re-composition, the spawned entity is updated to the latest provided value.
///
/// # Examples
///
/// ```no_run
/// use actuate::prelude::*;
/// use bevy::prelude::*;
///
/// #[derive(Data)]
/// struct Button {
///     label: String,
///     color: Color
/// }
///
/// impl Compose for Button {
///     fn compose(cx: Scope<Self>) -> impl Compose {
///         // Spawn an entity with a `Text` and `BackgroundColor` component.
///         spawn((Text::new(cx.me().label.clone()), BackgroundColor(cx.me().color)))
///     }
/// }
/// ```
pub fn spawn<'a, B>(bundle: B) -> Spawn<'a>
where
    B: Bundle + Clone,
{
    Spawn {
        spawn_fn: Arc::new(move |world, cell| {
            if let Some(entity) = cell {
                world.entity_mut(*entity).insert(bundle.clone());
            } else {
                *cell = Some(world.spawn(bundle.clone()).id())
            }
        }),
        content: (),
        target: None,
        observer_fns: Vec::new(),
        observer_guard: Arc::new(Mutex::new(true)),
        on_add: Cell::new(None),
        on_insert: Vec::new(),
    }
}

type SpawnFn = Arc<dyn Fn(&mut World, &mut Option<Entity>)>;

type ObserverFn<'a> = Box<dyn Fn(&mut EntityWorldMut) + 'a>;

type OnAddFn<'a> = Box<dyn FnOnce(EntityWorldMut) + 'a>;

type OnInsertFn<'a> = Box<dyn Fn(EntityWorldMut) + 'a>;

/// Composable to spawn an entity.
///
/// See [`spawn`] for more information.
#[must_use = "Composables do nothing unless composed or returned from other composables."]
pub struct Spawn<'a, C = ()> {
    spawn_fn: SpawnFn,
    content: C,
    target: Option<Entity>,
    observer_fns: Vec<ObserverFn<'a>>,
    on_add: Cell<Option<OnAddFn<'a>>>,
    on_insert: Vec<OnInsertFn<'a>>,
    observer_guard: Arc<Mutex<bool>>,
}

impl<'a, C> Spawn<'a, C> {
    /// Set the target entity to spawn the composition into.
    ///
    /// If `None`, this will use the composition's parent (if any).
    pub fn target(mut self, target: Entity) -> Self {
        self.target = Some(target);
        self
    }

    /// Set the child content.
    pub fn content<C2>(self, content: C2) -> Spawn<'a, C2> {
        Spawn {
            spawn_fn: self.spawn_fn,
            content,
            target: self.target,
            observer_fns: self.observer_fns,
            on_add: self.on_add,
            observer_guard: Arc::new(Mutex::new(false)),
            on_insert: self.on_insert,
        }
    }

    /// Set a function to be called when this entity is spawned.
    pub fn on_spawn<F>(self, f: F) -> Self
    where
        F: FnOnce(EntityWorldMut) + 'a,
    {
        self.on_add.set(Some(Box::new(f)));
        self
    }

    /// Add a function to be called on every insert.
    pub fn on_insert(mut self, f: impl Fn(EntityWorldMut) + 'a) -> Self {
        self.on_insert.push(Box::new(f));
        self
    }

    /// Add an observer to the spawned entity.
    pub fn observe<F, E, B, Marker>(mut self, observer: F) -> Self
    where
        F: SystemParamFunction<Marker, In = Trigger<'static, E, B>, Out = ()> + Send + Sync + 'a,
        E: Event,
        B: Bundle,
    {
        let cell = Cell::new(Some(observer));
        let guard = self.observer_guard.clone();

        self.observer_fns.push(Box::new(move |entity| {
            let mut observer = cell.take().unwrap();
            let guard = guard.clone();

            type SpawnObserveFn<'a, F, E, B, Marker> = Box<
                dyn FnMut(
                        Trigger<'_, E, B>,
                        ParamSet<'_, '_, (<F as SystemParamFunction<Marker>>::Param,)>,
                    ) + Send
                    + Sync
                    + 'a,
            >;

            let f: SpawnObserveFn<'a, F, E, B, Marker> = Box::new(move |trigger, mut params| {
                let guard = guard.lock().unwrap();
                if !*guard {
                    panic!("Actuate observer called after its scope was dropped.")
                }

                // Safety: The event will be accessed under a shortened lifetime.
                let trigger: Trigger<'static, E, B> = unsafe { mem::transmute(trigger) };
                observer.run(trigger, params.p0())
            });

            // Safety: The observer will be disabled after this scope is dropped.
            let f: SpawnObserveFn<'static, F, E, B, Marker> = unsafe { mem::transmute(f) };

            entity.observe(f);
        }));
        self
    }
}

unsafe impl<C: Data> Data for Spawn<'_, C> {}

impl<C: Compose> Compose for Spawn<'_, C> {
    fn compose(cx: Scope<Self>) -> impl Compose {
        let spawn_cx = use_context::<SpawnContext>(&cx);

        let is_initial = use_ref(&cx, || Cell::new(true));
        let entity = use_bundle_inner(&cx, |world, entity| {
            if let Some(target) = cx.me().target {
                *entity = Some(target);
            }

            (cx.me().spawn_fn)(world, entity);

            for f in &cx.me().on_insert {
                f(world.entity_mut(entity.unwrap()));
            }

            if is_initial.get() {
                let mut entity_mut = world.entity_mut(entity.unwrap());
                for f in &cx.me().observer_fns {
                    f(&mut entity_mut);
                }

                if let Some(f) = cx.me().on_add.take() {
                    f(entity_mut);
                }

                is_initial.set(false);
            }
        });

        use_provider(&cx, || {
            if cx.me().target.is_none() {
                if let Ok(parent_entity) = spawn_cx.map(|cx| cx.parent_entity) {
                    let world = unsafe { RuntimeContext::current().world_mut() };
                    world.entity_mut(parent_entity).add_child(entity);
                }
            }

            SpawnContext {
                parent_entity: entity,
            }
        });

        // Use the initial guard.
        let guard = use_ref(&cx, || cx.me().observer_guard.clone());
        use_drop(&cx, move || {
            *guard.lock().unwrap() = false;
        });

        unsafe { Signal::map_unchecked(cx.me(), |me| &me.content) }
    }
}
