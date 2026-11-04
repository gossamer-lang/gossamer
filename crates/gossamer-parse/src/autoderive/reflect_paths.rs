/// What a module declares under one name, as the reflection passes read it.
#[derive(Debug, Clone)]
enum IndexedItem {
    /// A struct or enum: its reflected `(name, type)` entries, `None` for a
    /// unit struct, and its type parameters.
    Type {
        entries: Option<Vec<(String, String)>>,
        named_struct: bool,
        params: Vec<String>,
    },
    /// A transparent `type Name = Path` alias, with the path it spells.
    Alias(Vec<String>),
    Fn,
    Module,
}

/// The items and imports of one module of a bundled program.
#[derive(Debug, Default)]
struct ModuleItems {
    items: HashMap<String, IndexedItem>,
    /// Bound name -> the path the `use` wrote, read from this module.
    imports: HashMap<String, Vec<String>>,
}

/// Every module of a bundled program with its items and imports, so a path
/// written in one module can be followed to the declaration it names the way
/// the resolver follows it: `crate` / `self` / `super` anchors, then the
/// module's own items and imports, then each enclosing module's.
#[derive(Debug, Default)]
struct ProgramIndex {
    modules: HashMap<Vec<String>, ModuleItems>,
    /// Top-level modules a bundled dependency is inlined as.
    dependency_roots: HashSet<String>,
}

/// Bounds alias and import chains, which a program may write in a cycle.
const RESOLVE_DEPTH: u32 = 32;

impl ProgramIndex {
    fn build(sf: &SourceFile) -> Self {
        let mut index = Self::default();
        index.add_items(&sf.items, &mut Vec::new());
        for decl in &sf.uses {
            let UseTarget::Module(path) = &decl.target else {
                continue;
            };
            let base: Vec<String> = path.segments.iter().map(|s| s.name.clone()).collect();
            let imports = &mut index.modules.entry(decl.module.clone()).or_default().imports;
            match &decl.list {
                None if !decl.glob => {
                    let Some(last) = base.last() else { continue };
                    let bound = decl.alias.as_ref().map_or(last, |a| &a.name).clone();
                    imports.insert(bound, base.clone());
                }
                None => {}
                Some(entries) => {
                    for entry in entries {
                        let mut full = base.clone();
                        full.extend(entry.prefix.iter().map(|s| s.name.clone()));
                        full.push(entry.name.name.clone());
                        let bound = entry.alias.as_ref().unwrap_or(&entry.name).name.clone();
                        imports.insert(bound, full);
                    }
                }
            }
        }
        index
    }

    fn add_items(&mut self, items: &[Item], module: &mut Vec<String>) {
        self.modules.entry(module.clone()).or_default();
        for item in items {
            if !gossamer_ast::cfg::item_is_active(&item.attrs) {
                continue;
            }
            let (name, indexed) = match &item.kind {
                ItemKind::Struct(decl) => (
                    &decl.name.name,
                    IndexedItem::Type {
                        entries: struct_entries(&decl.body),
                        named_struct: matches!(decl.body, StructBody::Named(_)),
                        params: param_names(&decl.generics),
                    },
                ),
                ItemKind::Enum(decl) => (
                    &decl.name.name,
                    IndexedItem::Type {
                        entries: Some(enum_entries(decl)),
                        named_struct: false,
                        params: param_names(&decl.generics),
                    },
                ),
                ItemKind::TypeAlias(decl) if !decl.nominal && decl.generics.params.is_empty() => {
                    let gossamer_ast::ty::TypeKind::Path(tp) = &decl.ty.kind else {
                        continue;
                    };
                    let path = tp.segments.iter().map(|s| s.name.name.clone()).collect();
                    (&decl.name.name, IndexedItem::Alias(path))
                }
                ItemKind::Fn(decl) => (&decl.name.name, IndexedItem::Fn),
                ItemKind::Mod(decl) => {
                    if module.is_empty()
                        && item
                            .attrs
                            .outer
                            .iter()
                            .any(|a| a.string_argument("dependency").is_some())
                    {
                        self.dependency_roots.insert(decl.name.name.clone());
                    }
                    if let ModBody::Inline(inner) = &decl.body {
                        module.push(decl.name.name.clone());
                        self.add_items(inner, module);
                        module.pop();
                    }
                    (&decl.name.name, IndexedItem::Module)
                }
                _ => continue,
            };
            self.modules
                .entry(module.clone())
                .or_default()
                .items
                .insert(name.clone(), indexed);
        }
    }

    fn item(&self, path: &[String]) -> Option<&IndexedItem> {
        let (name, module) = path.split_last()?;
        self.modules.get(module)?.items.get(name)
    }

    /// The root of the package `module` belongs to: the unit root, or the
    /// top-level module a bundled dependency is inlined as.
    fn package_root(&self, module: &[String]) -> Vec<String> {
        match module.first() {
            Some(head) if self.dependency_roots.contains(head) => vec![head.clone()],
            _ => Vec::new(),
        }
    }

    /// The absolute path of the item `segments`, written in `module`, names.
    fn resolve(&self, module: &[String], segments: &[String]) -> Option<Vec<String>> {
        self.resolve_at(module, segments, 0)
    }

    fn resolve_at(&self, module: &[String], segments: &[String], depth: u32) -> Option<Vec<String>> {
        let first = segments.first()?;
        if depth > RESOLVE_DEPTH {
            return None;
        }
        match first.as_str() {
            "crate" => self.walk(self.package_root(module), &segments[1..]),
            "self" => self.walk(module.to_vec(), &segments[1..]),
            "super" => {
                let ups = segments.iter().take_while(|s| *s == "super").count();
                let base = module.get(..module.len().checked_sub(ups)?)?.to_vec();
                self.walk(base, &segments[ups..])
            }
            _ => (0..=module.len()).rev().find_map(|level| {
                let scope = &module[..level];
                let items = self.modules.get(scope)?;
                if items.items.contains_key(first) {
                    return self.walk(scope.to_vec(), segments);
                }
                let imported = items.imports.get(first)?;
                let target = self.resolve_at(scope, imported, depth + 1)?;
                self.walk(target, &segments[1..])
            }),
        }
    }

    /// Follows `rest` down from `at` through declared modules.
    fn walk(&self, mut at: Vec<String>, rest: &[String]) -> Option<Vec<String>> {
        for (i, segment) in rest.iter().enumerate() {
            let item = self.modules.get(&at)?.items.get(segment)?;
            at.push(segment.clone());
            if i + 1 < rest.len() && !matches!(item, IndexedItem::Module) {
                return None;
            }
        }
        Some(at)
    }

    /// The struct or enum `segments`, written in `module`, names, through any
    /// transparent aliases.
    fn resolve_type(&self, module: &[String], segments: &[String]) -> Option<Vec<String>> {
        let mut path = self.resolve(module, segments)?;
        for _ in 0..RESOLVE_DEPTH {
            match self.item(&path)? {
                IndexedItem::Type { .. } => return Some(path),
                IndexedItem::Alias(target) => {
                    let home = &path[..path.len() - 1];
                    path = self.resolve(home, target)?;
                }
                IndexedItem::Fn | IndexedItem::Module => return None,
            }
        }
        None
    }

    /// `(name, type)` entries of a non-generic named-field struct, the shape a
    /// field loop unrolls over.
    fn struct_fields(&self, id: &[String]) -> Option<&[(String, String)]> {
        match self.item(id)? {
            IndexedItem::Type {
                entries: Some(entries),
                named_struct: true,
                params,
            } if params.is_empty() => Some(entries),
            _ => None,
        }
    }

    /// The spelling that names the type `id` from the unit root, and the
    /// dependency module the root has to import for it, if any.
    fn root_spelling(&self, id: &[String]) -> (String, Option<String>) {
        let dependency = id
            .first()
            .filter(|head| id.len() > 1 && self.dependency_roots.contains(*head))
            .cloned();
        let spelling = if dependency.is_some() {
            id.join("::")
        } else {
            format!("crate::{}", id.join("::"))
        };
        (spelling, dependency)
    }
}

/// Name of the reflection function for the type declared at `id`. A type at
/// the unit root keeps the bare `__gos_typeinfo_<Name>`; one in a module
/// carries the module path, so two modules' same-named types each reflect
/// their own fields. A generic instantiation's mangled argument spellings
/// follow the name.
fn reflect_fn_name(id: &[String], args: &[String]) -> String {
    let Some((name, module)) = id.split_last() else {
        return String::new();
    };
    let mut out = type_info_fn(name);
    if !args.is_empty() {
        out.push_str("__");
        out.push_str(&args.join("_"));
    }
    if !module.is_empty() {
        out.push_str("__in__");
        out.push_str(&module.join("__"));
    }
    out
}

/// Visits every item of a bundled program other than a module - functions,
/// impls, traits, consts, statics - with the module path it is declared in.
fn for_each_item_mut(
    items: &mut [Item],
    module: &mut Vec<String>,
    visit: &mut impl FnMut(&[String], &mut Item),
) {
    for item in items {
        if let ItemKind::Mod(decl) = &mut item.kind {
            if let ModBody::Inline(inner) = &mut decl.body {
                module.push(decl.name.name.clone());
                for_each_item_mut(inner, module, visit);
                module.pop();
            }
            continue;
        }
        visit(module, item);
    }
}

/// As [`for_each_item_mut`], reading the items.
fn for_each_item(items: &[Item], module: &mut Vec<String>, visit: &mut impl FnMut(&[String], &Item)) {
    for item in items {
        if let ItemKind::Mod(decl) = &item.kind {
            if let ModBody::Inline(inner) = &decl.body {
                module.push(decl.name.name.clone());
                for_each_item(inner, module, visit);
                module.pop();
            }
            continue;
        }
        visit(module, item);
    }
}
