use std::collections::HashSet;
use std::collections::BTreeMap;
use std::{cmp, fmt, hash};

use core::compiler::{BuildProfile, CompileMode};
use core::interning::InternedString;
use core::{Features, PackageId, PackageIdSpec, PackageSet, Shell};
use util::errors::CargoResultExt;
use util::lev_distance::lev_distance;
use util::toml::{ProfilePackageSpec, StringOrBool, TomlProfile, TomlProfiles, U32OrBool};
use util::{CargoResult, Config};

/// Collection of all user profiles.
#[derive(Clone, Debug)]
pub struct Profiles {
    dev: ProfileMaker,
    release: ProfileMaker,
    test: ProfileMaker,
    bench: ProfileMaker,
    doc: ProfileMaker,
    custom: BTreeMap<String, ProfileMaker>,
}

impl Profiles {
    pub fn new(
        profiles: Option<&TomlProfiles>,
        config: &Config,
        features: &Features,
        warnings: &mut Vec<String>,
    ) -> CargoResult<Profiles> {
        if let Some(profiles) = profiles {
            profiles.validate(features, warnings)?;
        }

        let config_profiles = config.profiles()?;
        config_profiles.validate(features, warnings)?;

        let mut profile_makers = Profiles {
            dev: ProfileMaker {
                default: Profile::default_dev(),
                toml: profiles.and_then(|p| p.dev.clone()),
                config: config_profiles.dev.clone(),
                inherits: vec![],
            },
            release: ProfileMaker {
                default: Profile::default_release(),
                toml: profiles.and_then(|p| p.release.clone()),
                inherits: vec![],
                config: config_profiles.release.clone(),
            },
            test: ProfileMaker {
                default: Profile::default_test(),
                toml: profiles.and_then(|p| p.test.clone()),
                config: None,
                inherits: vec![],
            },
            bench: ProfileMaker {
                default: Profile::default_bench(),
                toml: profiles.and_then(|p| p.bench.clone()),
                config: None,
                inherits: vec![],
            },
            doc: ProfileMaker {
                default: Profile::default_doc(),
                toml: profiles.and_then(|p| p.doc.clone()),
                config: None,
                inherits: vec![],
            },
            custom: BTreeMap::new(),
        };

        if let Some(profiles) = profiles {
            match &profiles.custom {
                None => {},
                Some(customs) => {
                    profile_makers.process_customs(customs)?;
                }
            }
        }

        Ok(profile_makers)
    }

    pub fn process_customs(&mut self, profiles: &BTreeMap<String, TomlProfile>)
        -> CargoResult<()>
    {
        for (name, profile) in profiles {
            let mut set = HashSet::new();
            let mut result = Vec::new();

            set.insert(name.as_str().to_owned());

            let mut maker = self.process_chain_custom(&profile, &mut set,
                                                      &mut result, profiles)?;
            result.reverse();
            maker.inherits = result;

            self.custom.insert(name.as_str().to_owned(), maker);
        }

        Ok(())
    }

    fn process_chain_custom(&mut self,
                            profile: &TomlProfile,
                            set: &mut HashSet<String>,
                            result: &mut Vec<TomlProfile>,
                            profiles: &BTreeMap<String, TomlProfile>)
        -> CargoResult<ProfileMaker>
    {
        result.push(profile.clone());
        match profile.inherits.as_ref().map(|x| x.as_str()) {
            Some("release") => {
                return Ok(self.release.clone());
            }
            Some("dev") => {
                return Ok(self.dev.clone());
            }
            Some(custom_name) => {
                let custom_name = custom_name.to_owned();
                if set.get(&custom_name).is_some() {
                    return Err(format_err!("Inheritance loop of custom profiles cycles with {}", custom_name));
                }

                set.insert(custom_name.clone());
                match profiles.get(&custom_name) {
                    None => {
                        return Err(format_err!("Custom profile {} not found in Cargo.toml", custom_name));
                    }
                    Some(parent) => {
                        self.process_chain_custom(parent, set, result, profiles)
                    }
                }
            }
            None => {
                Err(format_err!("An 'inherits' directive is needed for all custom profiles"))
            }
        }
    }


    /// Retrieve the profile for a target.
    /// `is_member` is whether or not this package is a member of the
    /// workspace.
    pub fn get_profile(
        &self,
        pkg_id: PackageId,
        is_member: bool,
        unit_for: UnitFor,
        mode: CompileMode,
        build_profile: BuildProfile,
    ) -> Profile {
        let maker = match mode {
            CompileMode::Test | CompileMode::Bench => {
                match &build_profile {
                    BuildProfile::Release => {
                        &self.bench
                    }
                    BuildProfile::Dev => {
                        &self.test
                    }
                    BuildProfile::Custom(name) => {
                        self.custom.get(name.as_str()).unwrap()
                    }
                }
            }
            CompileMode::Build
            | CompileMode::Check { .. }
            | CompileMode::Doctest
            | CompileMode::RunCustomBuild => {
                // Note: RunCustomBuild doesn't normally use this code path.
                // `build_unit_profiles` normally ensures that it selects the
                // ancestor's profile.  However `cargo clean -p` can hit this
                // path.
                match &build_profile {
                    BuildProfile::Release => {
                        &self.release
                    }
                    BuildProfile::Dev => {
                        &self.dev
                    }
                    BuildProfile::Custom(name) => {
                        self.custom.get(name.as_str()).unwrap()
                    }
                }
            }
            CompileMode::Doc { .. } => &self.doc,
        };
        let mut profile = maker.get_profile(Some(pkg_id), is_member, unit_for);
        // `panic` should not be set for tests/benches, or any of their
        // dependencies.
        if !unit_for.is_panic_ok() || mode.is_any_test() {
            profile.panic = None;
        }
        profile
    }

    /// The profile for *running* a `build.rs` script is only used for setting
    /// a few environment variables.  To ensure proper de-duplication of the
    /// running `Unit`, this uses a stripped-down profile (so that unrelated
    /// profile flags don't cause `build.rs` to needlessly run multiple
    /// times).
    pub fn get_profile_run_custom_build(&self, for_unit_profile: &Profile) -> Profile {
        let mut result = Profile::default();
        result.debuginfo = for_unit_profile.debuginfo;
        result.opt_level = for_unit_profile.opt_level;
        result
    }

    /// This returns a generic base profile. This is currently used for the
    /// `[Finished]` line.  It is not entirely accurate, since it doesn't
    /// select for the package that was actually built.
    pub fn base_profile(&self, build_profile: &BuildProfile) -> Profile {
        match &build_profile {
            BuildProfile::Release => {
                self.release.get_profile(None, true, UnitFor::new_normal())
            }
            BuildProfile::Dev => {
                self.dev.get_profile(None, true, UnitFor::new_normal())
            }
            BuildProfile::Custom(name) => {
                let r = self.custom.get(name.as_str()).unwrap();
                r.get_profile(None, true, UnitFor::new_normal())
            }
        }
    }

    /// Used to check for overrides for non-existing packages.
    pub fn validate_packages(&self, shell: &mut Shell, packages: &PackageSet) -> CargoResult<()> {
        self.dev.validate_packages(shell, packages)?;
        self.release.validate_packages(shell, packages)?;
        self.test.validate_packages(shell, packages)?;
        self.bench.validate_packages(shell, packages)?;
        self.doc.validate_packages(shell, packages)?;
        Ok(())
    }
}

/// An object used for handling the profile override hierarchy.
///
/// The precedence of profiles are (first one wins):
/// - Profiles in .cargo/config files (using same order as below).
/// - [profile.dev.overrides.name] - A named package.
/// - [profile.dev.overrides."*"] - This cannot apply to workspace members.
/// - [profile.dev.build-override] - This can only apply to `build.rs` scripts
///   and their dependencies.
/// - [profile.dev]
/// - Default (hard-coded) values.
#[derive(Debug, Clone)]
struct ProfileMaker {
    /// The starting, hard-coded defaults for the profile.
    default: Profile,
    /// The profile from the `Cargo.toml` manifest.
    toml: Option<TomlProfile>,

    /// Profiles from which we inherit, in the order from which
    /// we inherit.
    inherits: Vec<TomlProfile>,

    /// Profile loaded from `.cargo/config` files.
    config: Option<TomlProfile>,
}

impl ProfileMaker {
    fn get_profile(
        &self,
        pkg_id: Option<PackageId>,
        is_member: bool,
        unit_for: UnitFor,
    ) -> Profile {
        let mut profile = self.default;
        if let Some(ref toml) = self.toml {
            merge_toml(pkg_id, is_member, unit_for, &mut profile, toml);
        }
        for toml in &self.inherits {
            merge_toml(pkg_id, is_member, unit_for, &mut profile, toml);
        }
        if let Some(ref toml) = self.config {
            merge_toml(pkg_id, is_member, unit_for, &mut profile, toml);
        }
        profile
    }

    fn validate_packages(&self, shell: &mut Shell, packages: &PackageSet) -> CargoResult<()> {
        self.validate_packages_toml(shell, packages, &self.toml, true)?;
        self.validate_packages_toml(shell, packages, &self.config, false)?;
        Ok(())
    }

    fn validate_packages_toml(
        &self,
        shell: &mut Shell,
        packages: &PackageSet,
        toml: &Option<TomlProfile>,
        warn_unmatched: bool,
    ) -> CargoResult<()> {
        let toml = match *toml {
            Some(ref toml) => toml,
            None => return Ok(()),
        };
        let overrides = match toml.overrides {
            Some(ref overrides) => overrides,
            None => return Ok(()),
        };
        // Verify that a package doesn't match multiple spec overrides.
        let mut found = HashSet::new();
        for pkg_id in packages.package_ids() {
            let matches: Vec<&PackageIdSpec> = overrides
                .keys()
                .filter_map(|key| match *key {
                    ProfilePackageSpec::All => None,
                    ProfilePackageSpec::Spec(ref spec) => {
                        if spec.matches(pkg_id) {
                            Some(spec)
                        } else {
                            None
                        }
                    }
                })
                .collect();
            match matches.len() {
                0 => {}
                1 => {
                    found.insert(matches[0].clone());
                }
                _ => {
                    let specs = matches
                        .iter()
                        .map(|spec| spec.to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    bail!(
                        "multiple profile overrides in profile `{}` match package `{}`\n\
                         found profile override specs: {}",
                        self.default.name,
                        pkg_id,
                        specs
                    );
                }
            }
        }

        if !warn_unmatched {
            return Ok(());
        }
        // Verify every override matches at least one package.
        let missing_specs = overrides.keys().filter_map(|key| {
            if let ProfilePackageSpec::Spec(ref spec) = *key {
                if !found.contains(spec) {
                    return Some(spec);
                }
            }
            None
        });
        for spec in missing_specs {
            // See if there is an exact name match.
            let name_matches: Vec<String> = packages
                .package_ids()
                .filter_map(|pkg_id| {
                    if pkg_id.name().as_str() == spec.name() {
                        Some(pkg_id.to_string())
                    } else {
                        None
                    }
                })
                .collect();
            if name_matches.is_empty() {
                let suggestion = packages
                    .package_ids()
                    .map(|p| (lev_distance(spec.name(), &p.name()), p.name()))
                    .filter(|&(d, _)| d < 4)
                    .min_by_key(|p| p.0)
                    .map(|p| p.1);
                match suggestion {
                    Some(p) => shell.warn(format!(
                        "profile override spec `{}` did not match any packages\n\n\
                         Did you mean `{}`?",
                        spec, p
                    ))?,
                    None => shell.warn(format!(
                        "profile override spec `{}` did not match any packages",
                        spec
                    ))?,
                }
            } else {
                shell.warn(format!(
                    "version or URL in profile override spec `{}` does not \
                     match any of the packages: {}",
                    spec,
                    name_matches.join(", ")
                ))?;
            }
        }
        Ok(())
    }
}

fn merge_toml(
    pkg_id: Option<PackageId>,
    is_member: bool,
    unit_for: UnitFor,
    profile: &mut Profile,
    toml: &TomlProfile,
) {
    merge_profile(profile, toml);
    if unit_for.is_custom_build() {
        if let Some(ref build_override) = toml.build_override {
            merge_profile(profile, build_override);
        }
    }
    if let Some(ref overrides) = toml.overrides {
        if !is_member {
            if let Some(all) = overrides.get(&ProfilePackageSpec::All) {
                merge_profile(profile, all);
            }
        }
        if let Some(pkg_id) = pkg_id {
            let mut matches = overrides
                .iter()
                .filter_map(|(key, spec_profile)| match *key {
                    ProfilePackageSpec::All => None,
                    ProfilePackageSpec::Spec(ref s) => {
                        if s.matches(pkg_id) {
                            Some(spec_profile)
                        } else {
                            None
                        }
                    }
                });
            if let Some(spec_profile) = matches.next() {
                merge_profile(profile, spec_profile);
                // `validate_packages` should ensure that there are
                // no additional matches.
                assert!(
                    matches.next().is_none(),
                    "package `{}` matched multiple profile overrides",
                    pkg_id
                );
            }
        }
    }
}

fn merge_profile(profile: &mut Profile, toml: &TomlProfile) {
    if let Some(ref opt_level) = toml.opt_level {
        profile.opt_level = InternedString::new(&opt_level.0);
    }
    match toml.lto {
        Some(StringOrBool::Bool(b)) => profile.lto = Lto::Bool(b),
        Some(StringOrBool::String(ref n)) => profile.lto = Lto::Named(InternedString::new(n)),
        None => {}
    }
    if toml.codegen_units.is_some() {
        profile.codegen_units = toml.codegen_units;
    }
    match toml.debug {
        Some(U32OrBool::U32(debug)) => profile.debuginfo = Some(debug),
        Some(U32OrBool::Bool(true)) => profile.debuginfo = Some(2),
        Some(U32OrBool::Bool(false)) => profile.debuginfo = None,
        None => {}
    }
    if let Some(debug_assertions) = toml.debug_assertions {
        profile.debug_assertions = debug_assertions;
    }
    if let Some(rpath) = toml.rpath {
        profile.rpath = rpath;
    }
    if let Some(ref panic) = toml.panic {
        profile.panic = Some(InternedString::new(panic));
    }
    if let Some(overflow_checks) = toml.overflow_checks {
        profile.overflow_checks = overflow_checks;
    }
    if let Some(incremental) = toml.incremental {
        profile.incremental = incremental;
    }
}

/// Profile settings used to determine which compiler flags to use for a
/// target.
#[derive(Clone, Copy, Eq, PartialOrd, Ord)]
pub struct Profile {
    pub name: &'static str,
    pub opt_level: InternedString,
    pub lto: Lto,
    // None = use rustc default
    pub codegen_units: Option<u32>,
    pub debuginfo: Option<u32>,
    pub debug_assertions: bool,
    pub overflow_checks: bool,
    pub rpath: bool,
    pub incremental: bool,
    pub panic: Option<InternedString>,
}

impl Default for Profile {
    fn default() -> Profile {
        Profile {
            name: "",
            opt_level: InternedString::new("0"),
            lto: Lto::Bool(false),
            codegen_units: None,
            debuginfo: None,
            debug_assertions: false,
            overflow_checks: false,
            rpath: false,
            incremental: false,
            panic: None,
        }
    }
}

compact_debug! {
    impl fmt::Debug for Profile {
        fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
            let (default, default_name) = match self.name {
                "dev" => (Profile::default_dev(), "default_dev()"),
                "release" => (Profile::default_release(), "default_release()"),
                "test" => (Profile::default_test(), "default_test()"),
                "bench" => (Profile::default_bench(), "default_bench()"),
                "doc" => (Profile::default_doc(), "default_doc()"),
                _ => (Profile::default(), "default()"),
            };
            [debug_the_fields(
                name
                opt_level
                lto
                codegen_units
                debuginfo
                debug_assertions
                overflow_checks
                rpath
                incremental
                panic
            )]
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "Profile({})", self.name)
    }
}

impl hash::Hash for Profile {
    fn hash<H>(&self, state: &mut H)
    where
        H: hash::Hasher,
    {
        self.comparable().hash(state);
    }
}

impl cmp::PartialEq for Profile {
    fn eq(&self, other: &Self) -> bool {
        self.comparable() == other.comparable()
    }
}

impl Profile {
    fn default_dev() -> Profile {
        Profile {
            name: "dev",
            debuginfo: Some(2),
            debug_assertions: true,
            overflow_checks: true,
            incremental: true,
            ..Profile::default()
        }
    }

    fn default_release() -> Profile {
        Profile {
            name: "release",
            opt_level: InternedString::new("3"),
            ..Profile::default()
        }
    }

    fn default_test() -> Profile {
        Profile {
            name: "test",
            ..Profile::default_dev()
        }
    }

    fn default_bench() -> Profile {
        Profile {
            name: "bench",
            ..Profile::default_release()
        }
    }

    fn default_doc() -> Profile {
        Profile {
            name: "doc",
            ..Profile::default_dev()
        }
    }

    /// Compare all fields except `name`, which doesn't affect compilation.
    /// This is necessary for `Unit` deduplication for things like "test" and
    /// "dev" which are essentially the same.
    fn comparable(
        &self,
    ) -> (
        &InternedString,
        &Lto,
        &Option<u32>,
        &Option<u32>,
        &bool,
        &bool,
        &bool,
        &bool,
        &Option<InternedString>,
    ) {
        (
            &self.opt_level,
            &self.lto,
            &self.codegen_units,
            &self.debuginfo,
            &self.debug_assertions,
            &self.overflow_checks,
            &self.rpath,
            &self.incremental,
            &self.panic,
        )
    }
}

/// The link-time-optimization setting.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash, PartialOrd, Ord)]
pub enum Lto {
    /// False = no LTO
    /// True = "Fat" LTO
    Bool(bool),
    /// Named LTO settings like "thin".
    Named(InternedString),
}

/// Flags used in creating `Unit`s to indicate the purpose for the target, and
/// to ensure the target's dependencies have the correct settings.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct UnitFor {
    /// A target for `build.rs` or any of its dependencies.  This enables
    /// `build-override` profiles for these targets.
    custom_build: bool,
    /// This is true if it is *allowed* to set the `panic` flag. Currently
    /// this is false for test/bench targets and all their dependencies, and
    /// "for_host" units such as proc-macro and custom build scripts and their
    /// dependencies.
    panic_ok: bool,
}

impl UnitFor {
    /// A unit for a normal target/dependency (i.e. not custom build,
    /// proc-macro/plugin, or test/bench).
    pub fn new_normal() -> UnitFor {
        UnitFor {
            custom_build: false,
            panic_ok: true,
        }
    }

    /// A unit for a custom build script or its dependencies.
    pub fn new_build() -> UnitFor {
        UnitFor {
            custom_build: true,
            panic_ok: false,
        }
    }

    /// A unit for a proc-macro or compiler plugin or their dependencies.
    pub fn new_compiler() -> UnitFor {
        UnitFor {
            custom_build: false,
            panic_ok: false,
        }
    }

    /// A unit for a test/bench target or their dependencies.
    pub fn new_test() -> UnitFor {
        UnitFor {
            custom_build: false,
            panic_ok: false,
        }
    }

    /// Create a variant based on `for_host` setting.
    ///
    /// When `for_host` is true, this clears `panic_ok` in a sticky fashion so
    /// that all its dependencies also have `panic_ok=false`.
    pub fn with_for_host(self, for_host: bool) -> UnitFor {
        UnitFor {
            custom_build: self.custom_build,
            panic_ok: self.panic_ok && !for_host,
        }
    }

    /// Returns true if this unit is for a custom build script or one of its
    /// dependencies.
    pub fn is_custom_build(self) -> bool {
        self.custom_build
    }

    /// Returns true if this unit is allowed to set the `panic` compiler flag.
    pub fn is_panic_ok(self) -> bool {
        self.panic_ok
    }

    /// All possible values, used by `clean`.
    pub fn all_values() -> &'static [UnitFor] {
        static ALL: [UnitFor; 3] = [
            UnitFor {
                custom_build: false,
                panic_ok: true,
            },
            UnitFor {
                custom_build: true,
                panic_ok: false,
            },
            UnitFor {
                custom_build: false,
                panic_ok: false,
            },
        ];
        &ALL
    }
}

/// Profiles loaded from .cargo/config files.
#[derive(Clone, Debug, Deserialize, Default)]
pub struct ConfigProfiles {
    dev: Option<TomlProfile>,
    release: Option<TomlProfile>,
}

impl ConfigProfiles {
    pub fn validate(&self, features: &Features, warnings: &mut Vec<String>) -> CargoResult<()> {
        if let Some(ref profile) = self.dev {
            profile
                .validate("dev", features, warnings)
                .chain_err(|| format_err!("config profile `profile.dev` is not valid"))?;
        }
        if let Some(ref profile) = self.release {
            profile
                .validate("release", features, warnings)
                .chain_err(|| format_err!("config profile `profile.release` is not valid"))?;
        }
        Ok(())
    }
}
