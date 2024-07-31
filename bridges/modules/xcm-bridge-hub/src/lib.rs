// Copyright 2019-2021 Parity Technologies (UK) Ltd.
// This file is part of Parity Bridges Common.

// Parity Bridges Common is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Parity Bridges Common is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Parity Bridges Common.  If not, see <http://www.gnu.org/licenses/>.

//! Module that adds XCM support to bridge pallets. The pallet allows to dynamically
//! open and close bridges between local (to this pallet location) and remote XCM
//! destinations.
//!
//! Every bridge between two XCM locations has a dedicated lane in associated
//! messages pallet. Assuming that this pallet is deployed at the bridge hub
//! parachain and there's a similar pallet at the bridged network, the dynamic
//! bridge lifetime is as follows:
//!
//! 1) the sibling parachain opens a XCMP channel with this bridge hub;
//!
//! 2) the sibling parachain funds its sovereign parachain account at this bridge hub. It shall hold
//!    enough funds to pay for the bridge (see `BridgeDeposit`);
//!
//! 3) the sibling parachain opens the bridge by sending XCM `Transact` instruction with the
//!    `open_bridge` call. The `BridgeDeposit` amount is reserved on the sovereign account of
//!    sibling parachain;
//!
//! 4) at the other side of the bridge, the same thing (1, 2, 3) happens. Parachains that need to
//!    connect over the bridge need to coordinate the moment when they start sending messages over
//!    the bridge. Otherwise they may lose messages and/or bundled assets;
//!
//! 5) when either side wants to close the bridge, it sends the XCM `Transact` with the
//!    `close_bridge` call. The bridge is closed immediately if there are no queued messages.
//!    Otherwise, the owner must repeat the `close_bridge` call to prune all queued messages first.
//!
//! The pallet doesn't provide any mechanism for graceful closure, because it always involves
//! some contract between two connected chains and the bridge hub knows nothing about that. It
//! is the task for the connected chains to make sure that all required actions are completed
//! before the closure. In the end, the bridge hub can't even guarantee that all messages that
//! are delivered to the destination, are processed in the way their sender expects. So if we
//! can't guarantee that, we shall not care about more complex procedures and leave it to the
//! participating parties.

#![warn(missing_docs)]
#![cfg_attr(not(feature = "std"), no_std)]

use bp_messages::{LaneId, LaneState, MessageNonce};
use bp_runtime::{AccountIdOf, BalanceOf, RangeInclusiveExt};
use bp_xcm_bridge_hub::{
	Bridge, BridgeId, BridgeLocations, BridgeLocationsError, BridgeState, LocalXcmChannelManager,
};
use frame_support::{traits::fungible::MutateHold, DefaultNoBound};
use frame_system::Config as SystemConfig;
use pallet_bridge_messages::{Config as BridgeMessagesConfig, LanesManagerError};
use sp_runtime::traits::Zero;
use sp_std::{boxed::Box, vec::Vec};
use xcm::prelude::*;
use xcm_builder::DispatchBlob;
use xcm_executor::traits::ConvertLocation;

pub use bp_xcm_bridge_hub::XcmAsPlainPayload;
pub use dispatcher::XcmBlobMessageDispatchResult;
pub use exporter::PalletAsHaulBlobExporter;
pub use pallet::*;

mod dispatcher;
mod exporter;
mod mock;

/// The target that will be used when publishing logs related to this pallet.
pub const LOG_TARGET: &str = "runtime::bridge-xcm";

#[frame_support::pallet]
pub mod pallet {
	use super::*;
	use frame_support::{pallet_prelude::*, traits::tokens::Precision};
	use frame_system::pallet_prelude::{BlockNumberFor, *};

	/// The reason for this pallet placing a hold on funds.
	#[pallet::composite_enum]
	pub enum HoldReason<I: 'static = ()> {
		/// The funds are held as a deposit for opened bridge.
		#[codec(index = 0)]
		BridgeDeposit,
	}

	#[pallet::config]
	#[pallet::disable_frame_system_supertrait_check]
	pub trait Config<I: 'static = ()>:
		BridgeMessagesConfig<Self::BridgeMessagesPalletInstance>
	{
		/// The overarching event type.
		type RuntimeEvent: From<Event<Self, I>>
			+ IsType<<Self as frame_system::Config>::RuntimeEvent>;

		/// Runtime's universal location.
		type UniversalLocation: Get<InteriorLocation>;
		// TODO: https://github.com/paritytech/parity-bridges-common/issues/1666 remove `ChainId` and
		// replace it with the `NetworkId` - then we'll be able to use
		// `T as pallet_bridge_messages::Config<T::BridgeMessagesPalletInstance>::BridgedChain::NetworkId`
		/// Bridged network as relative location of bridged `GlobalConsensus`.
		#[pallet::constant]
		type BridgedNetwork: Get<Location>;
		/// Associated messages pallet instance that bridges us with the
		/// `BridgedNetworkId` consensus.
		type BridgeMessagesPalletInstance: 'static;

		/// Price of single message export to the bridged consensus (`Self::BridgedNetwork`).
		type MessageExportPrice: Get<Assets>;
		/// Checks the XCM version for the destination.
		type DestinationVersion: GetVersion;

		/// The origin that is allowed to call privileged operations on the pallet, e.g. open/close
		/// bridge for location that coresponds to `Self::BridgeOriginAccountIdConverter` and
		/// `Self::BridgedNetwork`.
		type AdminOrigin: EnsureOrigin<<Self as SystemConfig>::RuntimeOrigin>;
		/// A set of XCM locations within local consensus system that are allowed to open
		/// bridges with remote destinations.
		type OpenBridgeOrigin: EnsureOrigin<
			<Self as SystemConfig>::RuntimeOrigin,
			Success = Location,
		>;
		/// A converter between a multi-location and a sovereign account.
		type BridgeOriginAccountIdConverter: ConvertLocation<AccountIdOf<ThisChainOf<Self, I>>>;

		/// Amount of this chain native tokens that is reserved on the sibling parachain account
		/// when bridge open request is registered.
		#[pallet::constant]
		type BridgeDeposit: Get<BalanceOf<ThisChainOf<Self, I>>>;
		/// Currency used to pay for bridge registration.
		type Currency: MutateHold<
			AccountIdOf<ThisChainOf<Self, I>>,
			Balance = BalanceOf<ThisChainOf<Self, I>>,
			Reason = Self::RuntimeHoldReason,
		>;
		/// The overarching runtime hold reason.
		type RuntimeHoldReason: From<HoldReason<I>>;

		/// Local XCM channel manager.
		type LocalXcmChannelManager: LocalXcmChannelManager;
		/// XCM-level dispatcher for inbound bridge messages.
		type BlobDispatcher: DispatchBlob;
	}

	/// An alias for the bridge metadata.
	pub type BridgeOf<T, I> = Bridge<ThisChainOf<T, I>>;
	/// An alias for this chain.
	pub type ThisChainOf<T, I> =
		pallet_bridge_messages::ThisChainOf<T, <T as Config<I>>::BridgeMessagesPalletInstance>;
	/// An alias for the associated lanes manager.
	pub type LanesManagerOf<T, I> =
		pallet_bridge_messages::LanesManager<T, <T as Config<I>>::BridgeMessagesPalletInstance>;

	#[pallet::pallet]
	pub struct Pallet<T, I = ()>(PhantomData<(T, I)>);

	#[pallet::hooks]
	impl<T: Config<I>, I: 'static> Hooks<BlockNumberFor<T>> for Pallet<T, I> {
		fn integrity_test() {
			assert!(
				Self::bridged_network_id().is_ok(),
				"Configured `T::BridgedNetwork`: {:?} does not contain `GlobalConsensus` junction with `NetworkId`",
				T::BridgedNetwork::get()
			)
		}

		#[cfg(feature = "try-runtime")]
		fn try_state(_n: BlockNumberFor<T>) -> Result<(), sp_runtime::TryRuntimeError> {
			Self::do_try_state()
		}
	}

	#[pallet::call]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Open a bridge between two locations.
		///
		/// The caller must be within the `T::OpenBridgeOrigin` filter (presumably: a sibling
		/// parachain or a parent relay chain). The `bridge_destination_universal_location` must be
		/// a destination within the consensus of the `T::BridgedNetwork` network.
		///
		/// The `BridgeDeposit` amount is reserved on the caller account. This deposit
		/// is unreserved after bridge is closed.
		///
		/// The states after this call: bridge is `Opened`, outbound lane is `Opened`, inbound lane
		/// is `Opened`.
		#[pallet::call_index(0)]
		#[pallet::weight(Weight::zero())] // TODO:(bridges-v2) - https://github.com/paritytech/polkadot-sdk/pull/4949 - add benchmarks impl - FAIL-CI
		pub fn open_bridge(
			origin: OriginFor<T>,
			bridge_destination_universal_location: Box<VersionedInteriorLocation>,
		) -> DispatchResult {
			// check and compute required bridge locations and laneId
			let xcm_version = bridge_destination_universal_location.identify_version();
			let locations =
				Self::bridge_locations_from_origin(origin, bridge_destination_universal_location)?;
			let lane_id = locations.calculate_lane_id(xcm_version).map_err(|e| {
				log::trace!(
					target: LOG_TARGET,
					"calculate_lane_id error: {e:?}",
				);
				Error::<T, I>::BridgeLocations(e)
			})?;

			// reserve balance on the parachain sovereign account
			let deposit = T::BridgeDeposit::get();
			let bridge_owner_account = T::BridgeOriginAccountIdConverter::convert_location(
				locations.bridge_origin_relative_location(),
			)
			.ok_or(Error::<T, I>::InvalidBridgeOriginAccount)?;
			T::Currency::hold(&HoldReason::BridgeDeposit.into(), &bridge_owner_account, deposit)
				.map_err(|_| Error::<T, I>::FailedToReserveBridgeDeposit)?;

			// save bridge metadata
			Bridges::<T, I>::try_mutate(locations.bridge_id(), |bridge| match bridge {
				Some(_) => Err(Error::<T, I>::BridgeAlreadyExists),
				None => {
					*bridge = Some(BridgeOf::<T, I> {
						bridge_origin_relative_location: Box::new(
							locations.bridge_origin_relative_location().clone().into(),
						),
						bridge_origin_universal_location: Box::new(
							locations.bridge_origin_universal_location().clone().into(),
						),
						bridge_destination_universal_location: Box::new(
							locations.bridge_destination_universal_location().clone().into(),
						),
						state: BridgeState::Opened,
						bridge_owner_account,
						reserve: deposit,
						lane_id,
					});
					Ok(())
				},
			})?;
			// save lane to bridge mapping
			LaneToBridge::<T, I>::try_mutate(lane_id, |bridge| match bridge {
				Some(_) => Err(Error::<T, I>::BridgeAlreadyExists),
				None => {
					*bridge = Some(locations.bridge_id().clone());
					Ok(())
				},
			})?;

			// create new lanes. Under normal circumstances, following calls shall never fail
			let lanes_manager = LanesManagerOf::<T, I>::new();
			lanes_manager
				.create_inbound_lane(lane_id)
				.map_err(Error::<T, I>::LanesManager)?;
			lanes_manager
				.create_outbound_lane(lane_id)
				.map_err(Error::<T, I>::LanesManager)?;

			// write something to log
			log::trace!(
				target: LOG_TARGET,
				"Bridge {:?} between {:?} and {:?} has been opened using lane_id: {lane_id:?}",
				locations.bridge_id(),
				locations.bridge_origin_universal_location(),
				locations.bridge_destination_universal_location(),
			);

			// deposit `BridgeOpened` event
			Self::deposit_event(Event::<T, I>::BridgeOpened {
				bridge_id: locations.bridge_id().clone(),
				bridge_deposit: Some(deposit),
				local_endpoint: Box::new(locations.bridge_origin_universal_location().clone()),
				remote_endpoint: Box::new(
					locations.bridge_destination_universal_location().clone(),
				),
				lane_id,
			});

			Ok(())
		}

		/// Try to close the bridge.
		///
		/// Can only be called by the "owner" of this side of the bridge, meaning that the
		/// inbound XCM channel with the local origin chain is working.
		///
		/// Closed bridge is a bridge without any traces in the runtime storage. So this method
		/// first tries to prune all queued messages at the outbound lane. When there are no
		/// outbound messages left, outbound and inbound lanes are purged. After that, funds
		/// are returned back to the owner of this side of the bridge.
		///
		/// The number of messages that we may prune in a single call is limited by the
		/// `may_prune_messages` argument. If there are more messages in the queue, the method
		/// prunes exactly `may_prune_messages` and exits early. The caller may call it again
		/// until outbound queue is depleted and get his funds back.
		///
		/// The states after this call: everything is either `Closed`, or purged from the
		/// runtime storage.
		#[pallet::call_index(1)]
		#[pallet::weight(Weight::zero())] // TODO:(bridges-v2) - https://github.com/paritytech/polkadot-sdk/pull/4949 - add benchmarks impl - FAIL-CI
		pub fn close_bridge(
			origin: OriginFor<T>,
			bridge_destination_universal_location: Box<VersionedInteriorLocation>,
			may_prune_messages: MessageNonce,
		) -> DispatchResult {
			// compute required bridge locations
			let locations =
				Self::bridge_locations_from_origin(origin, bridge_destination_universal_location)?;

			// TODO: https://github.com/paritytech/parity-bridges-common/issues/1760 - may do refund here, if
			// bridge/lanes are already closed + for messages that are not pruned

			// update bridge metadata - this also guarantees that the bridge is in the proper state
			let bridge =
				Bridges::<T, I>::try_mutate_exists(locations.bridge_id(), |bridge| match bridge {
					Some(bridge) => {
						bridge.state = BridgeState::Closed;
						Ok(bridge.clone())
					},
					None => Err(Error::<T, I>::UnknownBridge),
				})?;

			// close inbound and outbound lanes
			let lanes_manager = LanesManagerOf::<T, I>::new();
			let mut inbound_lane = lanes_manager
				.any_state_inbound_lane(bridge.lane_id)
				.map_err(Error::<T, I>::LanesManager)?;
			let mut outbound_lane = lanes_manager
				.any_state_outbound_lane(bridge.lane_id)
				.map_err(Error::<T, I>::LanesManager)?;

			// now prune queued messages
			let mut pruned_messages = 0;
			for _ in outbound_lane.queued_messages() {
				if pruned_messages == may_prune_messages {
					break
				}

				outbound_lane.remove_oldest_unpruned_message();
				pruned_messages += 1;
			}

			// if there are outbound messages in the queue, just update states and early exit
			if !outbound_lane.queued_messages().is_empty() {
				// update lanes state. Under normal circumstances, following calls shall never fail
				inbound_lane.set_state(LaneState::Closed);
				outbound_lane.set_state(LaneState::Closed);

				// write something to log
				let enqueued_messages = outbound_lane.queued_messages().saturating_len();
				log::trace!(
					target: LOG_TARGET,
					"Bridge {:?} between {:?} and {:?} is closing lane_id: {:?}. {} messages remaining",
					locations.bridge_id(),
					locations.bridge_origin_universal_location(),
					locations.bridge_destination_universal_location(),
					bridge.lane_id,
					enqueued_messages,
				);

				// deposit the `ClosingBridge` event
				Self::deposit_event(Event::<T, I>::ClosingBridge {
					bridge_id: locations.bridge_id().clone(),
					lane_id: bridge.lane_id,
					pruned_messages,
					enqueued_messages,
				});

				return Ok(())
			}

			// else we have pruned all messages, so lanes and the bridge itself may gone
			inbound_lane.purge();
			outbound_lane.purge();
			Bridges::<T, I>::remove(locations.bridge_id());
			LaneToBridge::<T, I>::remove(bridge.lane_id);

			// return deposit
			let released_deposit = T::Currency::release(
				&HoldReason::BridgeDeposit.into(),
				&bridge.bridge_owner_account,
				bridge.reserve,
				Precision::BestEffort,
			)
			.map_err(|e| {
				// we can't do anything here - looks like funds have been (partially) unreserved
				// before by someone else. Let's not fail, though - it'll be worse for the caller
				log::error!(
					target: LOG_TARGET,
					"Failed to unreserve during the bridge {:?} closure with error: {e:?}",
					locations.bridge_id(),
				);
				e
			})
			.ok();

			// write something to log
			log::trace!(
				target: LOG_TARGET,
				"Bridge {:?} between {:?} and {:?} has closed lane_id: {:?}, the bridge deposit {released_deposit:?} was returned",
				locations.bridge_id(),
				bridge.lane_id,
				locations.bridge_origin_universal_location(),
				locations.bridge_destination_universal_location(),
			);

			// deposit the `BridgePruned` event
			Self::deposit_event(Event::<T, I>::BridgePruned {
				bridge_id: locations.bridge_id().clone(),
				lane_id: bridge.lane_id,
				bridge_deposit: released_deposit,
				pruned_messages,
			});

			Ok(())
		}
	}

	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Return bridge endpoint locations and dedicated lane identifier. This method converts
		/// runtime `origin` argument to relative `Location` using the `T::OpenBridgeOrigin`
		/// converter.
		pub fn bridge_locations_from_origin(
			origin: OriginFor<T>,
			bridge_destination_universal_location: Box<VersionedInteriorLocation>,
		) -> Result<Box<BridgeLocations>, sp_runtime::DispatchError> {
			Self::bridge_locations(
				T::OpenBridgeOrigin::ensure_origin(origin)?,
				(*bridge_destination_universal_location)
					.try_into()
					.map_err(|_| Error::<T, I>::UnsupportedXcmVersion)?,
			)
		}

		/// Return bridge endpoint locations and dedicated **bridge** identifier (`BridgeId`).
		pub fn bridge_locations(
			bridge_origin_relative_location: Location,
			bridge_destination_universal_location: InteriorLocation,
		) -> Result<Box<BridgeLocations>, sp_runtime::DispatchError> {
			BridgeLocations::bridge_locations(
				T::UniversalLocation::get(),
				bridge_origin_relative_location,
				bridge_destination_universal_location,
				Self::bridged_network_id()?,
			)
			.map_err(|e| {
				log::trace!(
					target: LOG_TARGET,
					"bridge_locations error: {e:?}",
				);
				Error::<T, I>::BridgeLocations(e).into()
			})
		}

		/// Return bridge metadata by lane_id
		pub fn bridge_by_lane_id(lane_id: &LaneId) -> Option<(BridgeId, BridgeOf<T, I>)> {
			LaneToBridge::<T, I>::get(lane_id)
				.and_then(|bridge_id| Self::bridge(bridge_id).map(|bridge| (bridge_id, bridge)))
		}
	}

	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Returns some `NetworkId` if contains `GlobalConsensus` junction.
		fn bridged_network_id() -> Result<NetworkId, sp_runtime::DispatchError> {
			match T::BridgedNetwork::get().take_first_interior() {
				Some(GlobalConsensus(network)) => Ok(network),
				_ => Err(Error::<T, I>::BridgeLocations(
					BridgeLocationsError::InvalidBridgeDestination,
				)
				.into()),
			}
		}
	}

	#[cfg(any(test, feature = "try-runtime", feature = "std"))]
	impl<T: Config<I>, I: 'static> Pallet<T, I> {
		/// Ensure the correctness of the state of this pallet.
		pub fn do_try_state() -> Result<(), sp_runtime::TryRuntimeError> {
			use sp_std::collections::btree_set::BTreeSet;

			let mut lanes = BTreeSet::new();

			// check all known bridge configurations
			for (bridge_id, bridge) in Bridges::<T, I>::iter() {
				log::info!(target: LOG_TARGET, "Checking `do_try_state` for bridge_id: {bridge_id:?} and bridge: {bridge:?}");
				lanes.insert(Self::do_try_state_for_bridge(bridge_id, bridge)?);
			}
			ensure!(
				lanes.len() == Bridges::<T, I>::iter().count(),
				"Invalid `Bridges` configuration, probably two bridges handle the same laneId!"
			);
			ensure!(
				lanes.len() == LaneToBridge::<T, I>::iter().count(),
				"Invalid `LaneToBridge` configuration, probably missing or not removed laneId!"
			);

			Ok(())
		}

		/// Ensure the correctness of the state of the bridge.
		pub fn do_try_state_for_bridge(
			bridge_id: BridgeId,
			bridge: BridgeOf<T, I>,
		) -> Result<LaneId, sp_runtime::TryRuntimeError> {
			// check `BridgeId` points to the same `LaneId` and vice versa.
			ensure!(
				Some(bridge_id) == Self::lane_to_bridge(bridge.lane_id),
				"Found `LaneToBridge` inconsistency for bridge_id - missing mapping!"
			);

			// check that `locations` are convertible to the `latest` XCM.
			let bridge_origin_relative_location_as_latest: &Location =
				bridge.bridge_origin_relative_location.try_as().map_err(|_| {
					"`bridge.bridge_origin_relative_location` cannot be converted to the `latest` XCM, needs migration!"
				})?;
			let bridge_origin_universal_location_as_latest: &InteriorLocation = bridge.bridge_origin_universal_location
				.try_as()
				.map_err(|_| "`bridge.bridge_origin_universal_location` cannot be converted to the `latest` XCM, needs migration!")?;
			let bridge_destination_universal_location_as_latest: &InteriorLocation = bridge.bridge_destination_universal_location
				.try_as()
				.map_err(|_| "`bridge.bridge_destination_universal_location` cannot be converted to the `latest` XCM, needs migration!")?;

			// check `BridgeId` does not change
			ensure!(
				bridge_id == BridgeId::new(bridge_origin_universal_location_as_latest, bridge_destination_universal_location_as_latest),
				"`bridge_id` is different than calculated from `bridge_origin_universal_location_as_latest` and `bridge_destination_universal_location_as_latest`, needs migration!"
			);

			// check bridge account owner
			ensure!(
				T::BridgeOriginAccountIdConverter::convert_location(bridge_origin_relative_location_as_latest) == Some(bridge.bridge_owner_account),
				"`bridge.bridge_owner_account` is different than calculated from `bridge.bridge_origin_relative_location`, needs migration!"
			);

			Ok(bridge.lane_id)
		}
	}

	/// All registered bridges.
	#[pallet::storage]
	#[pallet::getter(fn bridge)]
	pub type Bridges<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Identity, BridgeId, BridgeOf<T, I>>;
	/// All registered `lane_id` and `bridge_id` mappings.
	#[pallet::storage]
	#[pallet::getter(fn lane_to_bridge)]
	pub type LaneToBridge<T: Config<I>, I: 'static = ()> =
		StorageMap<_, Identity, LaneId, BridgeId>;

	#[pallet::genesis_config]
	#[derive(DefaultNoBound)]
	pub struct GenesisConfig<T: Config<I>, I: 'static = ()> {
		/// Opened bridges.
		///
		/// Keep in mind that we are **NOT** reserving any amount for the bridges, opened at
		/// genesis. We are **NOT** opening lanes, used by this bridge. It all must be done using
		/// other pallets genesis configuration or some other means.
		pub opened_bridges: Vec<(Location, InteriorLocation)>,
		/// Dummy marker.
		pub phantom: sp_std::marker::PhantomData<(T, I)>,
	}

	#[pallet::genesis_build]
	impl<T: Config<I>, I: 'static> BuildGenesisConfig for GenesisConfig<T, I>
	where
		T: frame_system::Config<AccountId = AccountIdOf<ThisChainOf<T, I>>>,
	{
		fn build(&self) {
			for (bridge_origin_relative_location, bridge_destination_universal_location) in
				&self.opened_bridges
			{
				let locations = Pallet::<T, I>::bridge_locations(
					bridge_origin_relative_location.clone(),
					bridge_destination_universal_location.clone().into(),
				)
				.expect("Invalid genesis configuration");
				let lane_id =
					locations.calculate_lane_id(xcm::latest::VERSION).expect("Valid locations");
				let bridge_owner_account = T::BridgeOriginAccountIdConverter::convert_location(
					locations.bridge_origin_relative_location(),
				)
				.expect("Invalid genesis configuration");

				Bridges::<T, I>::insert(
					locations.bridge_id(),
					Bridge {
						bridge_origin_relative_location: Box::new(
							locations.bridge_origin_relative_location().clone().into(),
						),
						bridge_origin_universal_location: Box::new(
							locations.bridge_origin_universal_location().clone().into(),
						),
						bridge_destination_universal_location: Box::new(
							locations.bridge_destination_universal_location().clone().into(),
						),
						state: BridgeState::Opened,
						bridge_owner_account,
						reserve: Zero::zero(),
						lane_id,
					},
				);
				LaneToBridge::<T, I>::insert(lane_id, locations.bridge_id());

				let lanes_manager = LanesManagerOf::<T, I>::new();
				lanes_manager
					.create_inbound_lane(lane_id)
					.expect("Invalid genesis configuration");
				lanes_manager
					.create_outbound_lane(lane_id)
					.expect("Invalid genesis configuration");
			}
		}
	}

	#[pallet::event]
	#[pallet::generate_deposit(pub(super) fn deposit_event)]
	pub enum Event<T: Config<I>, I: 'static = ()> {
		/// The bridge between two locations has been opened.
		BridgeOpened {
			/// Bridge identifier.
			bridge_id: BridgeId,
			/// Amount of deposit held.
			bridge_deposit: Option<BalanceOf<ThisChainOf<T, I>>>,

			/// Universal location of local bridge endpoint.
			local_endpoint: Box<InteriorLocation>,
			/// Universal location of remote bridge endpoint.
			remote_endpoint: Box<InteriorLocation>,
			/// Lane identifier.
			lane_id: LaneId,
		},
		/// Bridge is going to be closed, but not yet fully pruned from the runtime storage.
		ClosingBridge {
			/// Bridge identifier.
			bridge_id: BridgeId,
			/// Lane identifier.
			lane_id: LaneId,
			/// Number of pruned messages during the close call.
			pruned_messages: MessageNonce,
			/// Number of enqueued messages that need to be pruned in follow up calls.
			enqueued_messages: MessageNonce,
		},
		/// Bridge has been closed and pruned from the runtime storage. It now may be reopened
		/// again by any participant.
		BridgePruned {
			/// Bridge identifier.
			bridge_id: BridgeId,
			/// Lane identifier.
			lane_id: LaneId,
			/// Amount of deposit released.
			bridge_deposit: Option<BalanceOf<ThisChainOf<T, I>>>,
			/// Number of pruned messages during the close call.
			pruned_messages: MessageNonce,
		},
	}

	#[pallet::error]
	pub enum Error<T, I = ()> {
		/// Bridge locations error.
		BridgeLocations(BridgeLocationsError),
		/// Invalid local bridge origin account.
		InvalidBridgeOriginAccount,
		/// The bridge is already registered in this pallet.
		BridgeAlreadyExists,
		/// The local origin already owns a maximal number of bridges.
		TooManyBridgesForLocalOrigin,
		/// Trying to close already closed bridge.
		BridgeAlreadyClosed,
		/// Lanes manager error.
		LanesManager(LanesManagerError),
		/// Trying to access unknown bridge.
		UnknownBridge,
		/// The bridge origin can't pay the required amount for opening the bridge.
		FailedToReserveBridgeDeposit,
		/// The version of XCM location argument is unsupported.
		UnsupportedXcmVersion,
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use mock::*;

	use bp_messages::LaneId;
	use frame_support::{assert_err, assert_noop, assert_ok, traits::fungible::Mutate, BoundedVec};
	use frame_system::{EventRecord, Phase};
	use sp_core::H256;
	use sp_runtime::TryRuntimeError;

	fn fund_origin_sovereign_account(locations: &BridgeLocations, balance: Balance) -> AccountId {
		let bridge_owner_account =
			LocationToAccountId::convert_location(locations.bridge_origin_relative_location())
				.unwrap();
		assert_ok!(Balances::mint_into(&bridge_owner_account, balance));
		bridge_owner_account
	}

	fn mock_open_bridge_from_with(
		origin: RuntimeOrigin,
		with: InteriorLocation,
	) -> (BridgeOf<TestRuntime, ()>, BridgeLocations) {
		let reserve = BridgeDeposit::get();
		let locations =
			XcmOverBridge::bridge_locations_from_origin(origin, Box::new(with.into())).unwrap();
		let lane_id = locations.calculate_lane_id(xcm::latest::VERSION).unwrap();
		let bridge_owner_account =
			fund_origin_sovereign_account(&locations, reserve + ExistentialDeposit::get());
		Balances::hold(&HoldReason::BridgeDeposit.into(), &bridge_owner_account, reserve).unwrap();

		let bridge = Bridge {
			bridge_origin_relative_location: Box::new(
				locations.bridge_origin_relative_location().clone().into(),
			),
			bridge_origin_universal_location: Box::new(
				locations.bridge_origin_universal_location().clone().into(),
			),
			bridge_destination_universal_location: Box::new(
				locations.bridge_destination_universal_location().clone().into(),
			),
			state: BridgeState::Opened,
			bridge_owner_account,
			reserve,
			lane_id,
		};
		Bridges::<TestRuntime, ()>::insert(locations.bridge_id(), bridge.clone());
		LaneToBridge::<TestRuntime, ()>::insert(bridge.lane_id, locations.bridge_id());

		let lanes_manager = LanesManagerOf::<TestRuntime, ()>::new();
		lanes_manager.create_inbound_lane(bridge.lane_id).unwrap();
		lanes_manager.create_outbound_lane(bridge.lane_id).unwrap();

		assert_ok!(XcmOverBridge::do_try_state());

		(bridge, *locations)
	}

	fn mock_open_bridge_from(
		origin: RuntimeOrigin,
	) -> (BridgeOf<TestRuntime, ()>, BridgeLocations) {
		mock_open_bridge_from_with(origin, bridged_asset_hub_universal_location())
	}

	fn enqueue_message(lane: LaneId) {
		let lanes_manager = LanesManagerOf::<TestRuntime, ()>::new();
		lanes_manager
			.active_outbound_lane(lane)
			.unwrap()
			.send_message(BoundedVec::try_from(vec![42]).expect("We craft valid messages"));
	}

	#[test]
	fn open_bridge_fails_if_origin_is_not_allowed() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::disallowed_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				sp_runtime::DispatchError::BadOrigin,
			);
		})
	}

	#[test]
	fn open_bridge_fails_if_origin_is_not_relative() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::parent_relay_chain_universal_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::BridgeLocations(
					BridgeLocationsError::InvalidBridgeOrigin
				),
			);

			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::sibling_parachain_universal_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::BridgeLocations(
					BridgeLocationsError::InvalidBridgeOrigin
				),
			);
		})
	}

	#[test]
	fn open_bridge_fails_if_destination_is_not_remote() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::parent_relay_chain_origin(),
					Box::new(
						[GlobalConsensus(RelayNetwork::get()), Parachain(BRIDGED_ASSET_HUB_ID)]
							.into()
					),
				),
				Error::<TestRuntime, ()>::BridgeLocations(BridgeLocationsError::DestinationIsLocal),
			);
		});
	}

	#[test]
	fn open_bridge_fails_if_outside_of_bridged_consensus() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::parent_relay_chain_origin(),
					Box::new(
						[
							GlobalConsensus(NonBridgedRelayNetwork::get()),
							Parachain(BRIDGED_ASSET_HUB_ID)
						]
						.into()
					),
				),
				Error::<TestRuntime, ()>::BridgeLocations(
					BridgeLocationsError::UnreachableDestination
				),
			);
		});
	}

	#[test]
	fn open_bridge_fails_if_origin_has_no_sovereign_account() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::origin_without_sovereign_account(),
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::InvalidBridgeOriginAccount,
			);
		});
	}

	#[test]
	fn open_bridge_fails_if_origin_sovereign_account_has_no_enough_funds() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::open_bridge(
					OpenBridgeOrigin::parent_relay_chain_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::FailedToReserveBridgeDeposit,
			);
		});
	}

	#[test]
	fn open_bridge_fails_if_it_already_exists() {
		run_test(|| {
			let origin = OpenBridgeOrigin::parent_relay_chain_origin();
			let locations = XcmOverBridge::bridge_locations_from_origin(
				origin.clone(),
				Box::new(bridged_asset_hub_universal_location().into()),
			)
			.unwrap();
			let lane_id = locations.calculate_lane_id(xcm::latest::VERSION).unwrap();
			fund_origin_sovereign_account(
				&locations,
				BridgeDeposit::get() + ExistentialDeposit::get(),
			);

			Bridges::<TestRuntime, ()>::insert(
				locations.bridge_id(),
				Bridge {
					bridge_origin_relative_location: Box::new(
						locations.bridge_origin_relative_location().clone().into(),
					),
					bridge_origin_universal_location: Box::new(
						locations.bridge_origin_universal_location().clone().into(),
					),
					bridge_destination_universal_location: Box::new(
						locations.bridge_destination_universal_location().clone().into(),
					),
					state: BridgeState::Opened,
					bridge_owner_account: [0u8; 32].into(),
					reserve: 0,
					lane_id,
				},
			);

			assert_noop!(
				XcmOverBridge::open_bridge(
					origin,
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::BridgeAlreadyExists,
			);
		})
	}

	#[test]
	fn open_bridge_fails_if_its_lanes_already_exists() {
		run_test(|| {
			let origin = OpenBridgeOrigin::parent_relay_chain_origin();
			let locations = XcmOverBridge::bridge_locations_from_origin(
				origin.clone(),
				Box::new(bridged_asset_hub_universal_location().into()),
			)
			.unwrap();
			let lane_id = locations.calculate_lane_id(xcm::latest::VERSION).unwrap();
			fund_origin_sovereign_account(
				&locations,
				BridgeDeposit::get() + ExistentialDeposit::get(),
			);

			let lanes_manager = LanesManagerOf::<TestRuntime, ()>::new();

			lanes_manager.create_inbound_lane(lane_id).unwrap();
			assert_noop!(
				XcmOverBridge::open_bridge(
					origin.clone(),
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::LanesManager(LanesManagerError::InboundLaneAlreadyExists),
			);

			lanes_manager.active_inbound_lane(lane_id).unwrap().purge();
			lanes_manager.create_outbound_lane(lane_id).unwrap();
			assert_noop!(
				XcmOverBridge::open_bridge(
					origin,
					Box::new(bridged_asset_hub_universal_location().into()),
				),
				Error::<TestRuntime, ()>::LanesManager(
					LanesManagerError::OutboundLaneAlreadyExists
				),
			);
		})
	}

	#[test]
	fn open_bridge_works() {
		run_test(|| {
			// in our test runtime, we expect that bridge may be opened by parent relay chain
			// and any sibling parachain
			let origins = [
				OpenBridgeOrigin::parent_relay_chain_origin(),
				OpenBridgeOrigin::sibling_parachain_origin(),
			];

			// check that every origin may open the bridge
			let lanes_manager = LanesManagerOf::<TestRuntime, ()>::new();
			let expected_reserve = BridgeDeposit::get();
			let existential_deposit = ExistentialDeposit::get();
			for origin in origins {
				// reset events
				System::set_block_number(1);
				System::reset_events();

				// compute all other locations
				let xcm_version = xcm::latest::VERSION;
				let locations = XcmOverBridge::bridge_locations_from_origin(
					origin.clone(),
					Box::new(
						VersionedInteriorLocation::from(bridged_asset_hub_universal_location())
							.into_version(xcm_version)
							.expect("valid conversion"),
					),
				)
				.unwrap();
				let lane_id = locations.calculate_lane_id(xcm_version).unwrap();

				// ensure that there's no bridge and lanes in the storage
				assert_eq!(Bridges::<TestRuntime, ()>::get(locations.bridge_id()), None);
				assert_eq!(
					lanes_manager.active_inbound_lane(lane_id).map(drop),
					Err(LanesManagerError::UnknownInboundLane)
				);
				assert_eq!(
					lanes_manager.active_outbound_lane(lane_id).map(drop),
					Err(LanesManagerError::UnknownOutboundLane)
				);
				assert_eq!(LaneToBridge::<TestRuntime, ()>::get(lane_id), None);

				// give enough funds to the sovereign account of the bridge origin
				let bridge_owner_account = fund_origin_sovereign_account(
					&locations,
					expected_reserve + existential_deposit,
				);
				assert_eq!(
					Balances::free_balance(&bridge_owner_account),
					expected_reserve + existential_deposit
				);
				assert_eq!(Balances::reserved_balance(&bridge_owner_account), 0);

				// now open the bridge
				assert_ok!(XcmOverBridge::open_bridge(
					origin,
					Box::new(locations.bridge_destination_universal_location().clone().into()),
				));

				// ensure that everything has been set up in the runtime storage
				assert_eq!(
					Bridges::<TestRuntime, ()>::get(locations.bridge_id()),
					Some(Bridge {
						bridge_origin_relative_location: Box::new(
							locations.bridge_origin_relative_location().clone().into()
						),
						bridge_origin_universal_location: Box::new(
							locations.bridge_origin_universal_location().clone().into(),
						),
						bridge_destination_universal_location: Box::new(
							locations.bridge_destination_universal_location().clone().into(),
						),
						state: BridgeState::Opened,
						bridge_owner_account: bridge_owner_account.clone(),
						reserve: expected_reserve,
						lane_id
					}),
				);
				assert_eq!(
					lanes_manager.active_inbound_lane(lane_id).map(|l| l.state()),
					Ok(LaneState::Opened)
				);
				assert_eq!(
					lanes_manager.active_outbound_lane(lane_id).map(|l| l.state()),
					Ok(LaneState::Opened)
				);
				assert_eq!(
					LaneToBridge::<TestRuntime, ()>::get(lane_id),
					Some(locations.bridge_id().clone())
				);
				assert_eq!(Balances::free_balance(&bridge_owner_account), existential_deposit);
				assert_eq!(Balances::reserved_balance(&bridge_owner_account), expected_reserve);

				// ensure that the proper event is deposited
				assert_eq!(
					System::events().last(),
					Some(&EventRecord {
						phase: Phase::Initialization,
						event: RuntimeEvent::XcmOverBridge(Event::BridgeOpened {
							bridge_id: locations.bridge_id().clone(),
							bridge_deposit: Some(BridgeDeposit::get()),
							local_endpoint: Box::new(
								locations.bridge_origin_universal_location().clone()
							),
							remote_endpoint: Box::new(
								locations.bridge_destination_universal_location().clone()
							),
							lane_id
						}),
						topics: vec![],
					}),
				);
			}
		});
	}

	#[test]
	fn close_bridge_fails_if_origin_is_not_allowed() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::close_bridge(
					OpenBridgeOrigin::disallowed_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
					0,
				),
				sp_runtime::DispatchError::BadOrigin,
			);
		})
	}

	#[test]
	fn close_bridge_fails_if_origin_is_not_relative() {
		run_test(|| {
			assert_noop!(
				XcmOverBridge::close_bridge(
					OpenBridgeOrigin::parent_relay_chain_universal_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
					0,
				),
				Error::<TestRuntime, ()>::BridgeLocations(
					BridgeLocationsError::InvalidBridgeOrigin
				),
			);

			assert_noop!(
				XcmOverBridge::close_bridge(
					OpenBridgeOrigin::sibling_parachain_universal_origin(),
					Box::new(bridged_asset_hub_universal_location().into()),
					0,
				),
				Error::<TestRuntime, ()>::BridgeLocations(
					BridgeLocationsError::InvalidBridgeOrigin
				),
			);
		})
	}

	#[test]
	fn close_bridge_fails_if_its_lanes_are_unknown() {
		run_test(|| {
			let origin = OpenBridgeOrigin::parent_relay_chain_origin();
			let (bridge, locations) = mock_open_bridge_from(origin.clone());

			let lanes_manager = LanesManagerOf::<TestRuntime, ()>::new();
			lanes_manager.any_state_inbound_lane(bridge.lane_id).unwrap().purge();
			assert_noop!(
				XcmOverBridge::close_bridge(
					origin.clone(),
					Box::new(locations.bridge_destination_universal_location().clone().into()),
					0,
				),
				Error::<TestRuntime, ()>::LanesManager(LanesManagerError::UnknownInboundLane),
			);
			lanes_manager.any_state_outbound_lane(bridge.lane_id).unwrap().purge();

			let (_, locations) = mock_open_bridge_from(origin.clone());
			lanes_manager.any_state_outbound_lane(bridge.lane_id).unwrap().purge();
			assert_noop!(
				XcmOverBridge::close_bridge(
					origin,
					Box::new(locations.bridge_destination_universal_location().clone().into()),
					0,
				),
				Error::<TestRuntime, ()>::LanesManager(LanesManagerError::UnknownOutboundLane),
			);
		});
	}

	#[test]
	fn close_bridge_works() {
		run_test(|| {
			let origin = OpenBridgeOrigin::parent_relay_chain_origin();
			let (bridge, locations) = mock_open_bridge_from(origin.clone());
			System::set_block_number(1);

			// remember owner balances
			let free_balance = Balances::free_balance(&bridge.bridge_owner_account);
			let reserved_balance = Balances::reserved_balance(&bridge.bridge_owner_account);

			// enqueue some messages
			for _ in 0..32 {
				enqueue_message(bridge.lane_id);
			}

			// now call the `close_bridge`, which will only partially prune messages
			assert_ok!(XcmOverBridge::close_bridge(
				origin.clone(),
				Box::new(locations.bridge_destination_universal_location().clone().into()),
				16,
			),);

			// as a result, the bridge and lanes are switched to the `Closed` state, some messages
			// are pruned, but funds are not unreserved
			let lanes_manager = LanesManagerOf::<TestRuntime, ()>::new();
			assert_eq!(
				Bridges::<TestRuntime, ()>::get(locations.bridge_id()).map(|b| b.state),
				Some(BridgeState::Closed)
			);
			assert_eq!(
				lanes_manager.any_state_inbound_lane(bridge.lane_id).unwrap().state(),
				LaneState::Closed
			);
			assert_eq!(
				lanes_manager.any_state_outbound_lane(bridge.lane_id).unwrap().state(),
				LaneState::Closed
			);
			assert_eq!(
				lanes_manager
					.any_state_outbound_lane(bridge.lane_id)
					.unwrap()
					.queued_messages()
					.checked_len(),
				Some(16)
			);
			assert_eq!(
				LaneToBridge::<TestRuntime, ()>::get(bridge.lane_id),
				Some(locations.bridge_id().clone())
			);
			assert_eq!(Balances::free_balance(&bridge.bridge_owner_account), free_balance);
			assert_eq!(Balances::reserved_balance(&bridge.bridge_owner_account), reserved_balance);
			assert_eq!(
				System::events().last(),
				Some(&EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::XcmOverBridge(Event::ClosingBridge {
						bridge_id: locations.bridge_id().clone(),
						lane_id: bridge.lane_id,
						pruned_messages: 16,
						enqueued_messages: 16,
					}),
					topics: vec![],
				}),
			);

			// now call the `close_bridge` again, which will only partially prune messages
			assert_ok!(XcmOverBridge::close_bridge(
				origin.clone(),
				Box::new(locations.bridge_destination_universal_location().clone().into()),
				8,
			),);

			// nothing is changed (apart from the pruned messages)
			assert_eq!(
				Bridges::<TestRuntime, ()>::get(locations.bridge_id()).map(|b| b.state),
				Some(BridgeState::Closed)
			);
			assert_eq!(
				lanes_manager.any_state_inbound_lane(bridge.lane_id).unwrap().state(),
				LaneState::Closed
			);
			assert_eq!(
				lanes_manager.any_state_outbound_lane(bridge.lane_id).unwrap().state(),
				LaneState::Closed
			);
			assert_eq!(
				lanes_manager
					.any_state_outbound_lane(bridge.lane_id)
					.unwrap()
					.queued_messages()
					.checked_len(),
				Some(8)
			);
			assert_eq!(
				LaneToBridge::<TestRuntime, ()>::get(bridge.lane_id),
				Some(locations.bridge_id().clone())
			);
			assert_eq!(Balances::free_balance(&bridge.bridge_owner_account), free_balance);
			assert_eq!(Balances::reserved_balance(&bridge.bridge_owner_account), reserved_balance);
			assert_eq!(
				System::events().last(),
				Some(&EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::XcmOverBridge(Event::ClosingBridge {
						bridge_id: locations.bridge_id().clone(),
						lane_id: bridge.lane_id,
						pruned_messages: 8,
						enqueued_messages: 8,
					}),
					topics: vec![],
				}),
			);

			// now call the `close_bridge` again that will prune all remaining messages and the
			// bridge
			assert_ok!(XcmOverBridge::close_bridge(
				origin,
				Box::new(locations.bridge_destination_universal_location().clone().into()),
				9,
			),);

			// there's no traces of bridge in the runtime storage and funds are unreserved
			assert_eq!(
				Bridges::<TestRuntime, ()>::get(locations.bridge_id()).map(|b| b.state),
				None
			);
			assert_eq!(
				lanes_manager.any_state_inbound_lane(bridge.lane_id).map(drop),
				Err(LanesManagerError::UnknownInboundLane)
			);
			assert_eq!(
				lanes_manager.any_state_outbound_lane(bridge.lane_id).map(drop),
				Err(LanesManagerError::UnknownOutboundLane)
			);
			assert_eq!(LaneToBridge::<TestRuntime, ()>::get(bridge.lane_id), None);
			assert_eq!(
				Balances::free_balance(&bridge.bridge_owner_account),
				free_balance + reserved_balance
			);
			assert_eq!(Balances::reserved_balance(&bridge.bridge_owner_account), 0);
			assert_eq!(
				System::events().last(),
				Some(&EventRecord {
					phase: Phase::Initialization,
					event: RuntimeEvent::XcmOverBridge(Event::BridgePruned {
						bridge_id: locations.bridge_id().clone(),
						lane_id: bridge.lane_id,
						bridge_deposit: Some(BridgeDeposit::get()),
						pruned_messages: 8,
					}),
					topics: vec![],
				}),
			);
		});
	}

	#[test]
	fn do_try_state_works() {
		use sp_runtime::Either;

		let bridge_origin_relative_location = SiblingLocation::get();
		let bridge_origin_universal_location = SiblingUniversalLocation::get();
		let bridge_destination_universal_location = BridgedUniversalDestination::get();
		let bridge_owner_account =
			LocationToAccountId::convert_location(&bridge_origin_relative_location)
				.expect("valid accountId");
		let bridge_owner_account_mismatch =
			LocationToAccountId::convert_location(&Location::parent()).expect("valid accountId");
		let bridge_id = BridgeId::new(
			&bridge_origin_universal_location,
			&bridge_destination_universal_location,
		);
		let bridge_id_mismatch = BridgeId::new(&InteriorLocation::Here, &InteriorLocation::Here);
		let lane_id = LaneId::from_inner(Either::Left(H256::default()));

		let test_bridge_state =
			|id, bridge, (lane_id, bridge_id), expected_error: Option<TryRuntimeError>| {
				Bridges::<TestRuntime, ()>::insert(id, bridge);
				LaneToBridge::<TestRuntime, ()>::insert(lane_id, bridge_id);

				let result = XcmOverBridge::do_try_state();
				if let Some(e) = expected_error {
					assert_err!(result, e);
				} else {
					assert_ok!(result);
				}
			};
		let cleanup = |bridge_id, lane_id| {
			Bridges::<TestRuntime, ()>::remove(bridge_id);
			LaneToBridge::<TestRuntime, ()>::remove(lane_id);
			assert_ok!(XcmOverBridge::do_try_state());
		};

		run_test(|| {
			// ok state
			test_bridge_state(
				bridge_id,
				Bridge {
					bridge_origin_relative_location: Box::new(VersionedLocation::from(
						bridge_origin_relative_location.clone(),
					)),
					bridge_origin_universal_location: Box::new(VersionedInteriorLocation::from(
						bridge_origin_universal_location.clone(),
					)),
					bridge_destination_universal_location: Box::new(
						VersionedInteriorLocation::from(
							bridge_destination_universal_location.clone(),
						),
					),
					state: BridgeState::Opened,
					bridge_owner_account: bridge_owner_account.clone(),
					reserve: Zero::zero(),
					lane_id,
				},
				(lane_id, bridge_id),
				None,
			);
			cleanup(bridge_id, lane_id);

			// error - missing `LaneToBridge` mapping
			test_bridge_state(
				bridge_id,
				Bridge {
					bridge_origin_relative_location: Box::new(VersionedLocation::from(
						bridge_origin_relative_location.clone(),
					)),
					bridge_origin_universal_location: Box::new(VersionedInteriorLocation::from(
						bridge_origin_universal_location.clone(),
					)),
					bridge_destination_universal_location: Box::new(
						VersionedInteriorLocation::from(
							bridge_destination_universal_location.clone(),
						),
					),
					state: BridgeState::Opened,
					bridge_owner_account: bridge_owner_account.clone(),
					reserve: Zero::zero(),
					lane_id,
				},
				(lane_id, bridge_id_mismatch),
				Some(TryRuntimeError::Other(
					"Found `LaneToBridge` inconsistency for bridge_id - missing mapping!",
				)),
			);
			cleanup(bridge_id, lane_id);

			// error bridge owner account cannot be calculated
			test_bridge_state(
				bridge_id,
				Bridge {
					bridge_origin_relative_location: Box::new(VersionedLocation::from(
						bridge_origin_relative_location.clone(),
					)),
					bridge_origin_universal_location: Box::new(VersionedInteriorLocation::from(
						bridge_origin_universal_location.clone(),
					)),
					bridge_destination_universal_location: Box::new(VersionedInteriorLocation::from(
						bridge_destination_universal_location.clone(),
					)),
					state: BridgeState::Opened,
					bridge_owner_account: bridge_owner_account_mismatch.clone(),
					reserve: Zero::zero(),
					lane_id,
				},
				(lane_id, bridge_id),
				Some(TryRuntimeError::Other("`bridge.bridge_owner_account` is different than calculated from `bridge.bridge_origin_relative_location`, needs migration!")),
			);
			cleanup(bridge_id, lane_id);

			// error when (bridge_origin_universal_location + bridge_destination_universal_location)
			// produces different `BridgeId`
			test_bridge_state(
				bridge_id_mismatch,
				Bridge {
					bridge_origin_relative_location: Box::new(VersionedLocation::from(
						bridge_origin_relative_location.clone(),
					)),
					bridge_origin_universal_location: Box::new(VersionedInteriorLocation::from(
						bridge_origin_universal_location.clone(),
					)),
					bridge_destination_universal_location: Box::new(VersionedInteriorLocation::from(
						bridge_destination_universal_location.clone(),
					)),
					state: BridgeState::Opened,
					bridge_owner_account: bridge_owner_account_mismatch.clone(),
					reserve: Zero::zero(),
					lane_id,
				},
				(lane_id, bridge_id_mismatch),
				Some(TryRuntimeError::Other("`bridge_id` is different than calculated from `bridge_origin_universal_location_as_latest` and `bridge_destination_universal_location_as_latest`, needs migration!")),
			);
			cleanup(bridge_id_mismatch, lane_id);
		});
	}
}
