use roc_collections::VecMap;
use roc_module::ident::{Ident, Lowercase};
use roc_module::symbol::{IdentId, IdentIds, ModuleId, Symbol};
use roc_problem::can::RuntimeError;
use roc_region::all::{Loc, Region};
use roc_types::subs::Variable;
use roc_types::types::{Alias, AliasKind, Type};

use crate::abilities::AbilitiesStore;

use bitvec::vec::BitVec;

#[derive(Clone, Debug)]
pub struct Scope {
    /// The type aliases currently in scope
    pub aliases: VecMap<Symbol, Alias>,

    /// The abilities currently in scope, and their implementors.
    pub abilities_store: AbilitiesStore,

    /// The current module being processed. This will be used to turn
    /// unqualified idents into Symbols.
    home: ModuleId,

    /// The first `exposed_ident_count` identifiers are exposed
    exposed_ident_count: usize,

    /// Identifiers that are imported (and introduced in the header)
    imports: Vec<(Ident, Symbol, Region)>,

    /// Identifiers that are in scope, and defined in the current module
    pub locals: ScopedIdentIds,
}

impl Scope {
    pub fn new(home: ModuleId, initial_ident_ids: IdentIds) -> Scope {
        let imports = Symbol::default_in_scope()
            .into_iter()
            .map(|(a, (b, c))| (a, b, c))
            .collect();

        Scope {
            home,
            exposed_ident_count: initial_ident_ids.len(),
            locals: ScopedIdentIds::from_ident_ids(home, initial_ident_ids),
            aliases: VecMap::default(),
            // TODO(abilities): default abilities in scope
            abilities_store: AbilitiesStore::default(),
            imports,
        }
    }

    pub fn lookup(&self, ident: &Ident, region: Region) -> Result<Symbol, RuntimeError> {
        match self.scope_contains(ident) {
            Some((symbol, _)) => Ok(symbol),
            None => {
                let error = RuntimeError::LookupNotInScope(
                    Loc {
                        region,
                        value: ident.clone(),
                    },
                    self.idents_in_scope().map(|v| v.as_ref().into()).collect(),
                );

                Err(error)
            }
        }
    }

    fn idents_in_scope(&self) -> impl Iterator<Item = Ident> + '_ {
        let it1 = self.locals.idents_in_scope();
        let it2 = self.imports.iter().map(|t| t.0.clone());

        it2.chain(it1)
    }

    pub fn lookup_alias(&self, symbol: Symbol) -> Option<&Alias> {
        self.aliases.get(&symbol)
    }

    /// Check if there is an opaque type alias referenced by `opaque_ref` referenced in the
    /// current scope. E.g. `@Age` must reference an opaque `Age` declared in this module, not any
    /// other!
    pub fn lookup_opaque_ref(
        &self,
        opaque_ref: &str,
        lookup_region: Region,
    ) -> Result<(Symbol, &Alias), RuntimeError> {
        debug_assert!(opaque_ref.starts_with('@'));
        let opaque = opaque_ref[1..].into();

        match self.locals.has_in_scope(&opaque) {
            Some((symbol, _)) => {
                match self.aliases.get(&symbol) {
                    None => Err(self.opaque_not_defined_error(opaque, lookup_region, None)),

                    Some(alias) => match alias.kind {
                        // The reference is to a proper alias like `Age : U32`, not an opaque type!
                        AliasKind::Structural => Err(self.opaque_not_defined_error(
                            opaque,
                            lookup_region,
                            Some(alias.header_region()),
                        )),
                        // All is good
                        AliasKind::Opaque => Ok((symbol, alias)),
                    },
                }
            }
            None => {
                for (import, _, decl_region) in self.imports.iter() {
                    if &opaque == import {
                        // The reference is to an opaque type declared in another module - this is
                        // illegal, as opaque types can only be wrapped/unwrapped in the scope they're
                        // declared.
                        return Err(RuntimeError::OpaqueOutsideScope {
                            opaque,
                            referenced_region: lookup_region,
                            imported_region: *decl_region,
                        });
                    }
                }

                Err(self.opaque_not_defined_error(opaque, lookup_region, None))
            }
        }
    }

    fn opaque_not_defined_error(
        &self,
        opaque: Ident,
        lookup_region: Region,
        opt_defined_alias: Option<Region>,
    ) -> RuntimeError {
        let opaques_in_scope = self
            .locals
            .ident_ids
            .ident_strs()
            .filter_map(|(ident_id, string)| {
                if string.is_empty() {
                    None
                } else {
                    Some((string, Symbol::new(self.home, ident_id)))
                }
            })
            .filter(|(_, sym)| {
                self.aliases
                    .get(sym)
                    .map(|alias| alias.kind)
                    .unwrap_or(AliasKind::Structural)
                    == AliasKind::Opaque
            })
            .map(|(v, _)| v.into())
            .collect();

        RuntimeError::OpaqueNotDefined {
            usage: Loc::at(lookup_region, opaque),
            opaques_in_scope,
            opt_defined_alias,
        }
    }

    /// Is an identifier in scope, either in the locals or imports
    fn scope_contains(&self, ident: &Ident) -> Option<(Symbol, Region)> {
        self.locals.has_in_scope(ident).or_else(|| {
            for (import, shadow, original_region) in self.imports.iter() {
                if ident == import {
                    return Some((*shadow, *original_region));
                }
            }

            None
        })
    }

    /// Introduce a new ident to scope.
    ///
    /// Returns Err if this would shadow an existing ident, including the
    /// Symbol and Region of the ident we already had in scope under that name.
    ///
    /// If this ident shadows an existing one, a new ident is allocated for the shadow. This is
    /// done so that all identifiers have unique symbols, which is important in particular when
    /// we generate code for value identifiers.
    /// If this behavior is undesirable, use [`Self::introduce_without_shadow_symbol`].
    pub fn introduce(
        &mut self,
        ident: Ident,
        region: Region,
    ) -> Result<Symbol, (Region, Loc<Ident>, Symbol)> {
        match self.introduce_without_shadow_symbol(&ident, region) {
            Ok(symbol) => Ok(symbol),
            Err((original_region, shadow)) => {
                let symbol = self.scopeless_symbol(&ident, region);

                Err((original_region, shadow, symbol))
            }
        }
    }

    /// Like [Self::introduce], but does not introduce a new symbol for the shadowing symbol.
    pub fn introduce_without_shadow_symbol(
        &mut self,
        ident: &Ident,
        region: Region,
    ) -> Result<Symbol, (Region, Loc<Ident>)> {
        match self.scope_contains(ident) {
            Some((_, original_region)) => {
                let shadow = Loc {
                    value: ident.clone(),
                    region,
                };
                Err((original_region, shadow))
            }
            None => Ok(self.commit_introduction(ident, region)),
        }
    }

    /// Like [Self::introduce], but handles the case of when an ident matches an ability member
    /// name. In such cases a new symbol is created for the ident (since it's expected to be a
    /// specialization of the ability member), but the ident is not added to the ident->symbol map.
    ///
    /// If the ident does not match an ability name, the behavior of this function is exactly that
    /// of `introduce`.
    #[allow(clippy::type_complexity)]
    pub fn introduce_or_shadow_ability_member(
        &mut self,
        ident: Ident,
        region: Region,
    ) -> Result<(Symbol, Option<Symbol>), (Region, Loc<Ident>, Symbol)> {
        match self.scope_contains(&ident) {
            Some((original_symbol, original_region)) => {
                let shadow_symbol = self.scopeless_symbol(&ident, region);

                if self.abilities_store.is_ability_member_name(original_symbol) {
                    self.abilities_store
                        .register_specializing_symbol(shadow_symbol, original_symbol);

                    Ok((shadow_symbol, Some(original_symbol)))
                } else {
                    // This is an illegal shadow.
                    let shadow = Loc {
                        value: ident.clone(),
                        region,
                    };

                    Err((original_region, shadow, shadow_symbol))
                }
            }
            None => {
                let new_symbol = self.commit_introduction(&ident, region);
                Ok((new_symbol, None))
            }
        }
    }

    fn commit_introduction(&mut self, ident: &Ident, region: Region) -> Symbol {
        // if the identifier is exposed, use the IdentId we already have for it
        // other modules depend on the symbol having that IdentId
        match self.locals.ident_ids.get_id(ident) {
            Some(ident_id) if ident_id.index() < self.exposed_ident_count => {
                let symbol = Symbol::new(self.home, ident_id);

                self.locals.in_scope.set(ident_id.index(), true);
                self.locals.regions[ident_id.index()] = region;

                symbol
            }
            _ => {
                let ident_id = self.locals.introduce_into_scope(ident, region);
                Symbol::new(self.home, ident_id)
            }
        }
    }

    /// Create a new symbol, but don't add it to the scope (yet)
    ///
    /// Used for record guards like { x: Just _ } where the `x` is not added to the scope,
    /// but also in other places where we need to create a symbol and we don't have the right
    /// scope information yet. An identifier can be introduced later, and will use the same IdentId
    pub fn scopeless_symbol(&mut self, ident: &Ident, region: Region) -> Symbol {
        self.locals.scopeless_symbol(ident, region)
    }

    /// Import a Symbol from another module into this module's top-level scope.
    ///
    /// Returns Err if this would shadow an existing ident, including the
    /// Symbol and Region of the ident we already had in scope under that name.
    pub fn import(
        &mut self,
        ident: Ident,
        symbol: Symbol,
        region: Region,
    ) -> Result<(), (Symbol, Region)> {
        for t in self.imports.iter() {
            if t.0 == ident {
                return Err((t.1, t.2));
            }
        }

        self.imports.push((ident, symbol, region));

        Ok(())
    }

    pub fn add_alias(
        &mut self,
        name: Symbol,
        region: Region,
        vars: Vec<Loc<(Lowercase, Variable)>>,
        typ: Type,
        kind: AliasKind,
    ) {
        let alias = create_alias(name, region, vars, typ, kind);
        self.aliases.insert(name, alias);
    }

    pub fn contains_alias(&mut self, name: Symbol) -> bool {
        self.aliases.contains_key(&name)
    }

    pub fn inner_scope<F, T>(&mut self, f: F) -> T
    where
        F: FnOnce(&mut Scope) -> T,
    {
        // store enough information to roll back to the original outer scope
        //
        // - abilities_store: ability definitions not allowed in inner scopes
        // - locals: everything introduced in the inner scope is marked as not in scope in the rollback
        // - aliases: stored in a VecMap, we just discard anything added in an inner scope
        // - exposed_ident_count: unchanged
        // - home: unchanged
        let aliases_count = self.aliases.len();
        let locals_snapshot = self.locals.snapshot();

        let result = f(self);

        self.aliases.truncate(aliases_count);
        self.locals.revert(locals_snapshot);

        result
    }

    pub fn register_debug_idents(&self) {
        self.home.register_debug_idents(&self.locals.ident_ids)
    }

    /// Generates a unique, new symbol like "$1" or "$5",
    /// using the home module as the module_id.
    ///
    /// This is used, for example, during canonicalization of an Expr::Closure
    /// to generate a unique symbol to refer to that closure.
    pub fn gen_unique_symbol(&mut self) -> Symbol {
        Symbol::new(self.home, self.locals.gen_unique())
    }
}

pub fn create_alias(
    name: Symbol,
    region: Region,
    vars: Vec<Loc<(Lowercase, Variable)>>,
    typ: Type,
    kind: AliasKind,
) -> Alias {
    let roc_types::types::VariableDetail {
        type_variables,
        lambda_set_variables,
        recursion_variables,
    } = typ.variables_detail();

    debug_assert!({
        let mut hidden = type_variables;

        for loc_var in vars.iter() {
            hidden.remove(&loc_var.value.1);
        }

        if !hidden.is_empty() {
            panic!(
                "Found unbound type variables {:?} \n in type alias {:?} {:?} : {:?}",
                hidden, name, &vars, &typ
            )
        }

        true
    });

    let lambda_set_variables: Vec<_> = lambda_set_variables
        .into_iter()
        .map(|v| roc_types::types::LambdaSet(Type::Variable(v)))
        .collect();

    Alias {
        region,
        type_variables: vars,
        lambda_set_variables,
        recursion_variables,
        typ,
        kind,
    }
}

#[derive(Clone, Debug)]
pub struct ScopedIdentIds {
    pub ident_ids: IdentIds,
    in_scope: BitVec,
    regions: Vec<Region>,
    home: ModuleId,
}

impl ScopedIdentIds {
    fn from_ident_ids(home: ModuleId, ident_ids: IdentIds) -> Self {
        let capacity = ident_ids.len();

        Self {
            in_scope: BitVec::repeat(false, capacity),
            ident_ids,
            regions: std::iter::repeat(Region::zero()).take(capacity).collect(),
            home,
        }
    }

    fn snapshot(&self) -> usize {
        debug_assert_eq!(self.ident_ids.len(), self.in_scope.len());

        self.ident_ids.len()
    }

    fn revert(&mut self, snapshot: usize) {
        for i in snapshot..self.in_scope.len() {
            self.in_scope.set(i, false);
        }
    }

    fn has_in_scope(&self, ident: &Ident) -> Option<(Symbol, Region)> {
        for ident_id in self.ident_ids.get_id_many(ident) {
            let index = ident_id.index();
            if self.in_scope[index] {
                return Some((Symbol::new(self.home, ident_id), self.regions[index]));
            }
        }

        None
    }

    fn idents_in_scope(&self) -> impl Iterator<Item = Ident> + '_ {
        self.ident_ids
            .ident_strs()
            .zip(self.in_scope.iter())
            .filter_map(|((_, string), keep)| {
                if *keep {
                    Some(Ident::from(string))
                } else {
                    None
                }
            })
    }

    fn introduce_into_scope(&mut self, ident_name: &Ident, region: Region) -> IdentId {
        let id = self.ident_ids.add_ident(ident_name);

        debug_assert_eq!(id.index(), self.in_scope.len());
        debug_assert_eq!(id.index(), self.regions.len());

        self.in_scope.push(true);
        self.regions.push(region);

        id
    }

    /// Adds an IdentId, but does not introduce it to the scope
    fn scopeless_symbol(&mut self, ident_name: &Ident, region: Region) -> Symbol {
        let id = self.ident_ids.add_ident(ident_name);

        debug_assert_eq!(id.index(), self.in_scope.len());
        debug_assert_eq!(id.index(), self.regions.len());

        self.in_scope.push(false);
        self.regions.push(region);

        Symbol::new(self.home, id)
    }

    fn gen_unique(&mut self) -> IdentId {
        let id = self.ident_ids.gen_unique();

        debug_assert_eq!(id.index(), self.in_scope.len());
        debug_assert_eq!(id.index(), self.regions.len());

        self.in_scope.push(false);
        self.regions.push(Region::zero());

        id
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use roc_module::symbol::ModuleIds;
    use roc_region::all::Position;

    use pretty_assertions::{assert_eq, assert_ne};

    #[test]
    fn scope_contains_introduced() {
        let _register_module_debug_names = ModuleIds::default();
        let mut scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let region = Region::zero();
        let ident = Ident::from("mezolit");

        assert!(scope.lookup(&ident, region).is_err());

        assert!(scope.introduce(ident.clone(), region).is_ok());

        assert!(scope.lookup(&ident, region).is_ok());
    }

    #[test]
    fn second_introduce_shadows() {
        let _register_module_debug_names = ModuleIds::default();
        let mut scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let region1 = Region::from_pos(Position { offset: 10 });
        let region2 = Region::from_pos(Position { offset: 20 });
        let ident = Ident::from("mezolit");

        assert!(scope.lookup(&ident, Region::zero()).is_err());

        let first = scope.introduce(ident.clone(), region1).unwrap();
        let (original_region, _ident, shadow_symbol) =
            scope.introduce(ident.clone(), region2).unwrap_err();

        scope.register_debug_idents();

        assert_ne!(first, shadow_symbol);
        assert_eq!(original_region, region1);

        let lookup = scope.lookup(&ident, Region::zero()).unwrap();

        assert_eq!(first, lookup);
    }

    #[test]
    fn inner_scope_does_not_influence_outer() {
        let _register_module_debug_names = ModuleIds::default();
        let mut scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let region = Region::zero();
        let ident = Ident::from("uránia");

        assert!(scope.lookup(&ident, region).is_err());

        scope.inner_scope(|inner| {
            assert!(inner.introduce(ident.clone(), region).is_ok());
        });

        assert!(scope.lookup(&ident, region).is_err());
    }

    #[test]
    fn default_idents_in_scope() {
        let _register_module_debug_names = ModuleIds::default();
        let scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let idents: Vec<_> = scope.idents_in_scope().collect();

        assert_eq!(
            &idents,
            &[
                Ident::from("Box"),
                Ident::from("Set"),
                Ident::from("Dict"),
                Ident::from("Str"),
                Ident::from("Ok"),
                Ident::from("False"),
                Ident::from("List"),
                Ident::from("True"),
                Ident::from("Err"),
            ]
        );
    }

    #[test]
    fn idents_with_inner_scope() {
        let _register_module_debug_names = ModuleIds::default();
        let mut scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let idents: Vec<_> = scope.idents_in_scope().collect();

        assert_eq!(
            &idents,
            &[
                Ident::from("Box"),
                Ident::from("Set"),
                Ident::from("Dict"),
                Ident::from("Str"),
                Ident::from("Ok"),
                Ident::from("False"),
                Ident::from("List"),
                Ident::from("True"),
                Ident::from("Err"),
            ]
        );

        let builtin_count = idents.len();

        let region = Region::zero();

        let ident1 = Ident::from("uránia");
        let ident2 = Ident::from("malmok");
        let ident3 = Ident::from("Járnak");

        scope.introduce(ident1.clone(), region).unwrap();
        scope.introduce(ident2.clone(), region).unwrap();
        scope.introduce(ident3.clone(), region).unwrap();

        let idents: Vec<_> = scope.idents_in_scope().collect();

        assert_eq!(
            &idents[builtin_count..],
            &[ident1.clone(), ident2.clone(), ident3.clone(),]
        );

        scope.inner_scope(|inner| {
            let ident4 = Ident::from("Ångström");
            let ident5 = Ident::from("Sirály");

            inner.introduce(ident4.clone(), region).unwrap();
            inner.introduce(ident5.clone(), region).unwrap();

            let idents: Vec<_> = inner.idents_in_scope().collect();

            assert_eq!(
                &idents[builtin_count..],
                &[
                    ident1.clone(),
                    ident2.clone(),
                    ident3.clone(),
                    ident4,
                    ident5
                ]
            );
        });

        let idents: Vec<_> = scope.idents_in_scope().collect();

        assert_eq!(&idents[builtin_count..], &[ident1, ident2, ident3,]);
    }

    #[test]
    fn import_is_in_scope() {
        let _register_module_debug_names = ModuleIds::default();
        let mut scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let ident = Ident::from("product");
        let symbol = Symbol::LIST_PRODUCT;
        let region = Region::zero();

        assert!(scope.lookup(&ident, region).is_err());

        assert!(scope.import(ident.clone(), symbol, region).is_ok());

        assert!(scope.lookup(&ident, region).is_ok());

        assert!(scope.idents_in_scope().any(|x| x == ident));
    }

    #[test]
    fn shadow_of_import() {
        let _register_module_debug_names = ModuleIds::default();
        let mut scope = Scope::new(ModuleId::ATTR, IdentIds::default());

        let ident = Ident::from("product");
        let symbol = Symbol::LIST_PRODUCT;

        let region1 = Region::from_pos(Position { offset: 10 });
        let region2 = Region::from_pos(Position { offset: 20 });

        scope.import(ident.clone(), symbol, region1).unwrap();

        let (original_region, _ident, shadow_symbol) =
            scope.introduce(ident.clone(), region2).unwrap_err();

        scope.register_debug_idents();

        assert_ne!(symbol, shadow_symbol);
        assert_eq!(original_region, region1);

        let lookup = scope.lookup(&ident, Region::zero()).unwrap();

        assert_eq!(symbol, lookup);
    }
}
