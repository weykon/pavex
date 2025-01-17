use ahash::{HashMap, HashMapExt};

use pavex_builder::{AppBlueprint, Lifecycle, Location, RawCallableIdentifiers};

use crate::web::interner::Interner;

pub(crate) type RawCallableIdentifierId = la_arena::Idx<RawCallableIdentifiers>;

pub(crate) struct RawCallableIdentifiersDb {
    interner: Interner<RawCallableIdentifiers>,
    id2locations: HashMap<RawCallableIdentifierId, Location>,
    id2lifecycle: HashMap<RawCallableIdentifierId, Lifecycle>,
}

impl RawCallableIdentifiersDb {
    pub fn build(bp: &AppBlueprint) -> Self {
        let mut interner = Interner::new();
        let mut id2locations = HashMap::new();
        let mut id2lifecycle = HashMap::new();

        for (route, request_handler) in &bp.router {
            let location = &bp.request_handler_locations[route];
            let id = interner.get_or_intern(request_handler.to_owned());
            id2locations.insert(id, location.to_owned());
            id2lifecycle.insert(id, Lifecycle::RequestScoped);
        }

        for (route, error_handler) in &bp.request_handlers_error_handlers {
            let location = &bp.request_error_handler_locations[route];
            let error_handler_id = interner.get_or_intern(error_handler.to_owned());
            id2locations.insert(error_handler_id, location.to_owned());
        }

        for (fallible_constructor, error_handler) in &bp.constructors_error_handlers {
            let location = &bp.error_handler_locations[fallible_constructor];
            let error_handler_id = interner.get_or_intern(error_handler.to_owned());
            id2locations.insert(error_handler_id, location.to_owned());
        }

        for constructor in &bp.constructors {
            let location = &bp.constructor_locations[constructor];
            let lifecycle = &bp.component_lifecycles[constructor];
            let id = interner.get_or_intern(constructor.to_owned());
            id2locations.insert(id, location.to_owned());
            id2lifecycle.insert(id, lifecycle.to_owned());
        }

        Self {
            interner,
            id2locations,
            id2lifecycle,
        }
    }

    pub fn get_lifecycle(&self, id: RawCallableIdentifierId) -> Option<&Lifecycle> {
        self.id2lifecycle.get(&id)
    }

    pub fn get_location(&self, id: RawCallableIdentifierId) -> &Location {
        &self.id2locations[&id]
    }
}

impl std::ops::Index<RawCallableIdentifierId> for RawCallableIdentifiersDb {
    type Output = RawCallableIdentifiers;

    fn index(&self, index: RawCallableIdentifierId) -> &Self::Output {
        &self.interner[index]
    }
}

impl std::ops::Index<&RawCallableIdentifiers> for RawCallableIdentifiersDb {
    type Output = RawCallableIdentifierId;

    fn index(&self, index: &RawCallableIdentifiers) -> &Self::Output {
        &self.interner[index]
    }
}
