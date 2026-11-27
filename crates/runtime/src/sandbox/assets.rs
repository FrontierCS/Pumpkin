//! The assets that a run's roles declare, published on their providers before any sandbox starts.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;

use provider::{AssetDigest, AssetRef, Assets, HeldAsset, Provider, PublishedAsset, RunTag};
use schema::ir::{Asset, AssetFiles, Name, Sha256Digest};
use schema::workflow::{At, ByProvider, ParseError, Placed, Placement, RoleId};
use sha2::{Digest as _, Sha256};
use stdx::Context as _;
use stdx::time::{Clock as _, Machine};

use crate::log::AssetPublished;
use crate::{Stopped, interruption};

pub(super) type Held = BTreeMap<RoleId, Vec<HeldAsset>>;

/// Each role that declares assets, with the provider that it is placed on, which publishes them
/// ([`holding`]).
pub(crate) struct Holding<'p>(BTreeMap<RoleId, &'p dyn Assets>);

/// The [`Holding`] of `placed` on `providers`.
///
/// Fails if a role that declares assets is placed on a provider that publishes none. That is the
/// provider's fault, as placing put the role in one of its classes that holds assets.
pub(crate) fn holding<'p>(
    placed: &Placed,
    providers: &ByProvider<&'p dyn Provider>,
) -> Result<Holding<'p>, String> {
    let mut holding = BTreeMap::new();
    for (role, sandbox) in placed.workflow().roles() {
        if sandbox.assets.is_empty() {
            continue;
        }
        let Placement {
            provider: on,
            class,
            declared: _,
        } = placed.placement(role);
        let provider: &'p dyn Provider = *providers.get(on);
        let Some(assets) = provider.assets() else {
            return Err(format!(
                "provider `{on}`'s class `{class}` holds assets, and the provider publishes none: \
                 expected only a provider that publishes assets to offer a class that holds them"
            ));
        };
        holding.insert(role, assets);
    }
    Ok(Holding(holding))
}

/// An error for each asset of a role in `holding` whose provider can't hold it at its path. The
/// error is at that path in the IR, and gives the provider's reason.
pub(crate) fn unheld(placed: &Placed, holding: &Holding<'_>) -> Vec<ParseError> {
    let workflow = placed.workflow();
    let mut unheld = Vec::new();
    for (&role, &assets) in &holding.0 {
        let sandbox = workflow.sandbox(role);
        let on = &placed.placement(role).provider;
        for (index, Asset { path, files: _ }) in sandbox.assets.iter().enumerate() {
            if let Err(why) = assets.holds(path) {
                let at = sandbox.at().token("assets").token(index).token("path");
                unheld.push(ParseError {
                    at: At {
                        ir: at,
                        source: None,
                    },
                    message: format!(
                        "role `{}` holds an asset at `{path}`, where provider `{on}` holds none: \
                         {why}",
                        sandbox.name
                    ),
                    label: None,
                    why: Vec::new(),
                    help: Vec::new(),
                });
            }
        }
    }
    unheld
}

/// Publishes the files of each asset of each role that `holding` gives, on the role's provider
/// there, for `run`. It publishes each digest once on each provider. Returns the assets that
/// those roles' sandboxes hold, by the role, and each publish.
pub(super) fn publish(
    run: &RunTag,
    placed: &Placed,
    holding: &Holding<'_>,
) -> Result<(Held, Vec<AssetPublished>), Stopped> {
    let workflow = placed.workflow();
    let mut published = BTreeMap::<(&Name, AssetDigest), AssetRef>::new();
    let (mut held, mut logged) = (BTreeMap::new(), Vec::new());
    for (&role, &assets) in &holding.0 {
        let sandbox = workflow.sandbox(role);
        let on = &placed.placement(role).provider;
        let mut held_by_role = Vec::new();
        for Asset { path, files } in &sandbox.assets {
            let reference = match published.entry((on, digest(files))) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => {
                    interruption::check()?;
                    let digest = &entry.key().1;
                    let began = Machine.now();
                    let publish = assets.publish_asset(run, digest, files);
                    let PublishedAsset { reference, fetched } = publish.context(format_args!(
                        "publishing role `{}`'s asset at `{path}`, {digest}, on provider `{on}` \
                         failed",
                        sandbox.name
                    ))?;
                    logged.push(AssetPublished {
                        provider: on.clone(),
                        digest: digest.clone(),
                        fetched,
                        took: Machine.now().saturating_sub(began),
                    });
                    entry.insert(reference).clone()
                }
            };
            let path = path.clone();
            held_by_role.push(HeldAsset { path, reference });
        }
        held.insert(role, held_by_role);
    }
    Ok((held, logged))
}

/// The digest of `files`, as [`AssetFiles`] defines it.
fn digest(files: &AssetFiles) -> AssetDigest {
    let mut files: Vec<_> = files.as_slice().iter().collect();
    // Bytewise, as `str` orders.
    files.sort_by(|a, b| a.path.as_str().cmp(b.path.as_str()));
    let mut lines = Sha256::new();
    for file in files {
        lines.update(format!("{}  {}\n", file.sha256.hex(), file.path));
    }
    AssetDigest::from(Sha256Digest::from(<[u8; 32]>::from(lines.finalize())))
}
