//! What each tool does, and which ones this server exposes.
//!
//! Two problems share one answer. First, a client sees a flat list of a hundred
//! tools and cannot tell `list_apps` from `remove_user` — so every call looks
//! equally risky and approval prompts stop meaning anything. Second, that same
//! list is a fixed context cost on every session, whether or not the agent will
//! ever touch Xcode Cloud.
//!
//! So each tool is classified once, here:
//!
//! - by [`Group`] — which domain module defined it, used by `ASC_TOOLS` to serve
//!   a subset;
//! - by [`Effect`] — what it does to the account, which becomes the MCP
//!   annotations a client uses to decide what needs confirming, and which tools
//!   survive `ASC_READ_ONLY`.
//!
//! [`Effect`] is derived from the verb a tool's name starts with rather than a
//! hand-maintained table of a hundred entries, because a table drifts the moment
//! someone adds a tool and forgets it. A name whose verb isn't recognised is
//! reported by [`Assembled::unclassified`] and fails a test, so the drift is
//! caught at build time rather than by a client that auto-approved a delete.

use std::collections::BTreeSet;

use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::ToolAnnotations;
use rmcp::service::MaybeSend;

use crate::config::ToolsConfig;

/// A domain of the App Store Connect API, matching the module that defines its
/// tools. The string form is what `ASC_TOOLS` accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Group {
    Generic,
    Apps,
    Iap,
    Subscriptions,
    Versions,
    Pricing,
    Availability,
    Submission,
    TestFlight,
    Provisioning,
    Assets,
    Offers,
    OfferCodes,
    Promotions,
    Reviews,
    Users,
    Events,
    XcodeCloud,
    Analytics,
    CustomProductPages,
    Market,
    Webhooks,
}

impl Group {
    pub const ALL: [Group; 22] = [
        Group::Generic,
        Group::Apps,
        Group::Iap,
        Group::Subscriptions,
        Group::Versions,
        Group::Pricing,
        Group::Availability,
        Group::Submission,
        Group::TestFlight,
        Group::Provisioning,
        Group::Assets,
        Group::Offers,
        Group::OfferCodes,
        Group::Promotions,
        Group::Reviews,
        Group::Users,
        Group::Events,
        Group::XcodeCloud,
        Group::Analytics,
        Group::CustomProductPages,
        Group::Market,
        Group::Webhooks,
    ];

    /// The everyday ship-an-app subset, for `ASC_TOOLS=core`.
    pub const CORE: [Group; 6] = [
        Group::Generic,
        Group::Apps,
        Group::Versions,
        Group::Assets,
        Group::TestFlight,
        Group::Submission,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Group::Generic => "generic",
            Group::Apps => "apps",
            Group::Iap => "iap",
            Group::Subscriptions => "subscriptions",
            Group::Versions => "versions",
            Group::Pricing => "pricing",
            Group::Availability => "availability",
            Group::Submission => "submission",
            Group::TestFlight => "testflight",
            Group::Provisioning => "provisioning",
            Group::Assets => "assets",
            Group::Offers => "offers",
            Group::OfferCodes => "offer-codes",
            Group::Promotions => "promotions",
            Group::Reviews => "reviews",
            Group::Users => "users",
            Group::Events => "events",
            Group::XcodeCloud => "xcode-cloud",
            Group::Analytics => "analytics",
            Group::CustomProductPages => "custom-product-pages",
            Group::Market => "market",
            Group::Webhooks => "webhooks",
        }
    }

    fn parse(name: &str) -> Option<Group> {
        let normalized = name.trim().to_ascii_lowercase().replace('_', "-");
        Group::ALL.into_iter().find(|g| g.as_str() == normalized)
    }
}

/// What calling a tool does to the App Store Connect account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect {
    /// Reads only.
    Read,
    /// Creates something new; calling it twice creates two of them.
    Create,
    /// Sets fields to given values; calling it twice leaves the same state.
    Update,
    /// Removes or invalidates something.
    Destructive,
    /// The generic escape hatch: whatever the caller's HTTP method does.
    MethodDependent,
}

impl Effect {
    /// Whether the tool can stay when the server is restricted to reads.
    ///
    /// The escape hatch survives because it refuses non-`GET` methods in that
    /// mode — and without it a read-only server could not reach the endpoints
    /// that have no curated tool.
    fn allowed_read_only(self) -> bool {
        matches!(self, Effect::Read | Effect::MethodDependent)
    }

    /// The MCP hints a client uses to decide what needs confirming.
    ///
    /// `open_world` is always true: every tool talks to Apple's servers.
    fn annotations(self, read_only_server: bool) -> ToolAnnotations {
        let base = ToolAnnotations::new().open_world(true);
        match self {
            Effect::Read => base.read_only(true).destructive(false).idempotent(true),
            Effect::Create => base.read_only(false).destructive(false).idempotent(false),
            Effect::Update => base.read_only(false).destructive(false).idempotent(true),
            Effect::Destructive => base.read_only(false).destructive(true).idempotent(true),
            // Restricted to GET when the server is read-only, so it is honestly
            // read-only then, and honestly dangerous when it is not.
            Effect::MethodDependent => base
                .read_only(read_only_server)
                .destructive(!read_only_server)
                .idempotent(false),
        }
    }
}

/// Leading verbs, mapped to what they do. Order matters only in that every
/// prefix here is unambiguous — no name starts with two of them.
const VERBS: &[(&str, Effect)] = &[
    ("list_", Effect::Read),
    ("get_", Effect::Read),
    // Removals and anything that takes a capability away.
    ("delete_", Effect::Destructive),
    ("remove_", Effect::Destructive),
    ("expire_", Effect::Destructive),
    ("disable_", Effect::Destructive),
    // Additive: each call produces another resource, submission, or invite.
    ("create_", Effect::Create),
    ("add_", Effect::Create),
    ("upload_", Effect::Create),
    ("generate_", Effect::Create),
    ("invite_", Effect::Create),
    ("register_", Effect::Create),
    ("submit_", Effect::Create),
    ("start_", Effect::Create),
    ("request_", Effect::Create),
    ("respond_", Effect::Create),
    ("download_", Effect::Read),
    ("search_", Effect::Read),
    ("analyze_", Effect::Read),
    // Setting named fields to given values.
    ("update_", Effect::Update),
    ("set_", Effect::Update),
    ("assign_", Effect::Update),
    ("enable_", Effect::Update),
    ("reorder_", Effect::Update),
];

/// Classify a tool by name, or return `None` if its verb isn't recognised.
pub fn classify(name: &str) -> Option<Effect> {
    match name {
        "api_execute" => Some(Effect::MethodDependent),
        "api_list" => Some(Effect::Read),
        "api_search" | "api_describe" => Some(Effect::Read),
        "search_tools" => Some(Effect::Read),
        "call_discovered_tool" => Some(Effect::MethodDependent),
        _ => VERBS
            .iter()
            .find(|(verb, _)| name.starts_with(verb))
            .map(|(_, effect)| *effect),
    }
}

/// The outcome of assembling the router: what is served, and what to warn about.
pub struct Assembled<S> {
    pub router: ToolRouter<S>,
    /// Groups actually being served, in declaration order.
    pub groups: Vec<Group>,
    /// Tools whose verb [`classify`] didn't recognise. Served without
    /// annotations (hiding a working tool would be worse), withheld entirely in
    /// read-only mode, and asserted empty by the test suite.
    pub unclassified: Vec<String>,
    /// Human-readable problems with `ASC_TOOLS`, for the startup log.
    pub warnings: Vec<String>,
    /// How many tools the configuration withheld.
    pub withheld: usize,
}

/// Build the served tool router from every domain router plus the configuration.
pub fn assemble<S>(routers: Vec<(Group, ToolRouter<S>)>, config: &ToolsConfig) -> Assembled<S>
where
    S: MaybeSend + 'static,
{
    let (enabled, warnings) = resolve_groups(config.groups.as_deref());

    let mut assembled = Assembled {
        router: ToolRouter::new(),
        groups: Vec::new(),
        unclassified: Vec::new(),
        warnings,
        withheld: 0,
    };

    for (group, router) in routers {
        if !enabled.contains(&group) {
            assembled.withheld += router.map.len();
            continue;
        }
        assembled.groups.push(group);
        for mut route in router {
            let effect = classify(&route.attr.name);
            match effect {
                Some(effect) => {
                    if config.read_only && !effect.allowed_read_only() {
                        assembled.withheld += 1;
                        continue;
                    }
                    route.attr.annotations = Some(effect.annotations(config.read_only));
                }
                None => {
                    assembled.unclassified.push(route.attr.name.to_string());
                    // Fail closed: an unclassified tool might write.
                    if config.read_only {
                        assembled.withheld += 1;
                        continue;
                    }
                }
            }
            assembled.router.add_route(route);
        }
    }
    assembled
}

/// Resolve the `ASC_TOOLS` value into a set of groups, collecting complaints
/// about anything unrecognised rather than refusing to start.
fn resolve_groups(raw: Option<&str>) -> (BTreeSet<Group>, Vec<String>) {
    let Some(raw) = raw else {
        return (Group::ALL.into_iter().collect(), Vec::new());
    };

    let mut enabled = BTreeSet::new();
    let mut warnings = Vec::new();
    let mut unknown = Vec::new();

    for token in raw.split(',').map(str::trim).filter(|t| !t.is_empty()) {
        match token.to_ascii_lowercase().as_str() {
            "all" => enabled.extend(Group::ALL),
            "core" => enabled.extend(Group::CORE),
            _ => match Group::parse(token) {
                Some(group) => {
                    enabled.insert(group);
                }
                None => unknown.push(token.to_string()),
            },
        }
    }

    if !unknown.is_empty() {
        warnings.push(format!(
            "ASC_TOOLS: ignoring unknown tool group(s) {}. Valid groups: all, core, {}.",
            unknown.join(", "),
            Group::ALL
                .iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if enabled.is_empty() {
        warnings.push(
            "ASC_TOOLS matched no valid group, so no tools are being served. \
Unset it to serve all tools."
                .to_string(),
        );
    }
    (enabled, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_verbs_are_read_only() {
        for name in ["list_apps", "get_app", "api_list", "download_report"] {
            assert_eq!(classify(name), Some(Effect::Read), "{name}");
        }
    }

    #[test]
    fn removals_are_flagged_destructive() {
        for name in [
            "delete_in_app_purchase",
            "remove_user",
            "expire_build",
            "disable_bundle_id_capability",
        ] {
            assert_eq!(classify(name), Some(Effect::Destructive), "{name}");
        }
    }

    #[test]
    fn creates_are_additive_and_not_idempotent() {
        for name in [
            "create_bundle_id",
            "add_beta_tester",
            "upload_app_screenshot",
        ] {
            assert_eq!(classify(name), Some(Effect::Create), "{name}");
        }
        let a = Effect::Create.annotations(false);
        assert_eq!(a.read_only_hint, Some(false));
        assert_eq!(a.destructive_hint, Some(false));
        assert_eq!(a.idempotent_hint, Some(false));
    }

    #[test]
    fn field_setters_are_idempotent_updates() {
        for name in ["update_app", "set_age_rating", "reorder_screenshots"] {
            assert_eq!(classify(name), Some(Effect::Update), "{name}");
        }
        let a = Effect::Update.annotations(false);
        assert_eq!(a.destructive_hint, Some(false));
        assert_eq!(a.idempotent_hint, Some(true));
    }

    #[test]
    fn unknown_verbs_are_not_guessed() {
        assert_eq!(classify("frobnicate_app"), None);
        assert_eq!(classify(""), None);
    }

    #[test]
    fn every_tool_talks_to_an_external_service() {
        for effect in [
            Effect::Read,
            Effect::Create,
            Effect::Update,
            Effect::Destructive,
            Effect::MethodDependent,
        ] {
            assert_eq!(effect.annotations(false).open_world_hint, Some(true));
        }
    }

    #[test]
    fn the_escape_hatch_is_honest_about_the_mode_it_is_in() {
        let open = Effect::MethodDependent.annotations(false);
        assert_eq!(open.read_only_hint, Some(false));
        assert_eq!(open.destructive_hint, Some(true));

        let restricted = Effect::MethodDependent.annotations(true);
        assert_eq!(restricted.read_only_hint, Some(true));
        assert_eq!(restricted.destructive_hint, Some(false));
    }

    #[test]
    fn read_only_mode_keeps_reads_and_the_escape_hatch() {
        assert!(Effect::Read.allowed_read_only());
        assert!(Effect::MethodDependent.allowed_read_only());
        for effect in [Effect::Create, Effect::Update, Effect::Destructive] {
            assert!(!effect.allowed_read_only(), "{effect:?}");
        }
    }

    #[test]
    fn no_group_name_collides_or_repeats() {
        let names: BTreeSet<&str> = Group::ALL.iter().map(|g| g.as_str()).collect();
        assert_eq!(names.len(), Group::ALL.len(), "duplicate group name");
    }

    #[test]
    fn unset_asc_tools_enables_everything() {
        let (groups, warnings) = resolve_groups(None);
        assert_eq!(groups.len(), Group::ALL.len());
        assert!(warnings.is_empty());
    }

    #[test]
    fn group_names_parse_with_either_separator_and_any_case() {
        let (groups, warnings) = resolve_groups(Some("TestFlight, offer_codes ,xcode-cloud"));
        assert_eq!(
            groups.into_iter().collect::<Vec<_>>(),
            vec![Group::TestFlight, Group::OfferCodes, Group::XcodeCloud]
        );
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn presets_expand() {
        let (all, _) = resolve_groups(Some("all"));
        assert_eq!(all.len(), Group::ALL.len());
        let (core, _) = resolve_groups(Some("core"));
        assert_eq!(core.len(), Group::CORE.len());
        assert!(core.contains(&Group::Generic));
        assert!(!core.contains(&Group::XcodeCloud));
    }

    #[test]
    fn unknown_groups_are_reported_without_discarding_valid_ones() {
        let (groups, warnings) = resolve_groups(Some("apps,nonsense"));
        assert_eq!(groups.into_iter().collect::<Vec<_>>(), vec![Group::Apps]);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("nonsense"), "{}", warnings[0]);
    }

    #[test]
    fn an_entirely_invalid_filter_serves_nothing_and_says_so() {
        // Failing open here would hand full write access to someone who asked
        // for a restricted server and made a typo.
        let (groups, warnings) = resolve_groups(Some("nope"));
        assert!(groups.is_empty());
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        assert!(warnings[1].contains("no tools"), "{}", warnings[1]);
    }
}
