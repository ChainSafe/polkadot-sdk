// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Functions that deal contract addresses.

use crate::{ensure, Config, Error, HoldReason, OriginalAccount};
use alloc::vec::Vec;
use core::marker::PhantomData;
use frame_support::traits::{fungible::MutateHold, tokens::Precision};
use sp_core::{Get, H160};
use sp_io::hashing::keccak_256;
use sp_runtime::{AccountId32, DispatchResult, Saturating};

/// Map between the native chain account id `T` and an Ethereum [`H160`].
///
/// This trait exists only to emulate specialization for different concrete
/// native account ids. **Not** to make the mapping user configurable. Hence
/// the trait is `Sealed` and depending on your runtime configuration you need
/// to pick either [`AccountId32Mapper`] or [`H160Mapper`]. Picking the wrong
/// one will result in a compilation error. No footguns here.
///
/// Please note that we assume that the native account is at least 20 bytes and
/// only implement this type for a `T` where this is the case. Luckily, this is the
/// case for all existing runtimes as of right now. Reasoning is that this will allow
/// us to reverse an address -> account_id mapping by just stripping the prefix.
///
/// We require the mapping to be reversible. Since we are potentially dealing with types of
/// different sizes one direction of the mapping is necessarily lossy. This requires the mapping to
/// make use of the [`OriginalAccount`] storage item to reverse the mapping.
pub trait AddressMapper<T: Config>: private::Sealed {
	/// Convert an account id to an ethereum address.
	fn to_address(account_id: &T::AccountId) -> H160;

	/// Convert an ethereum address to a native account id.
	fn to_account_id(address: &H160) -> T::AccountId;

	/// Same as [`Self::to_account_id`] but always returns the fallback account.
	///
	/// This skips the query into [`OriginalAccount`] and always returns the stateless
	/// fallback account. This is useful when we know for a fact that the `address`
	/// in question is originally a `H160`. This is usually only the case when we
	/// generated a new contract address.
	fn to_fallback_account_id(address: &H160) -> T::AccountId;

	/// Create a stateful mapping for `account_id`
	///
	/// This will enable `to_account_id` to map back to the original
	/// `account_id` instead of the fallback account id.
	fn map(account_id: &T::AccountId) -> DispatchResult;

	#[cfg(feature = "runtime-benchmarks")]
	fn bench_map(account_id: &T::AccountId) -> DispatchResult {
		Self::map(account_id)
	}

	/// Remove the mapping in order to reclaim the deposit.
	///
	/// There is no reason why one would unmap their `account_id` except
	/// for reclaiming the deposit.
	fn unmap(account_id: &T::AccountId) -> DispatchResult;

	/// Returns true if the `account_id` is usable as an origin.
	///
	/// This means either the `account_id` doesn't require a stateful mapping
	/// or a stateful mapping exists.
	fn is_mapped(account_id: &T::AccountId) -> bool;
}

mod private {
	pub trait Sealed {}
	impl<T> Sealed for super::AccountId32Mapper<T> {}
	impl<T> Sealed for super::H160Mapper<T> {}
	impl<T> Sealed for super::TestAccountMapper<T> {}
}

/// The mapper to be used if the account id is `AccountId32`.
///
/// It converts between addresses by either hash then truncate the last 12 bytes or
/// suffixing them. To recover the original account id of a hashed and truncated account id we use
/// [`OriginalAccount`] and will fall back to all `0xEE` if account was found. This means contracts
/// and plain wallets controlled by an `secp256k1` always have a `0xEE` suffixed account.
pub struct AccountId32Mapper<T>(PhantomData<T>);

/// The mapper to be used if the account id is `H160`.
///
/// It just trivially returns its inputs and doesn't make use of any state.
pub struct H160Mapper<T>(PhantomData<T>);

/// An account mapper that can be used for testing u64 account ids.
pub struct TestAccountMapper<T>(PhantomData<T>);

impl<T> AddressMapper<T> for AccountId32Mapper<T>
where
	T: Config<AccountId = AccountId32>,
{
	fn to_address(account_id: &AccountId32) -> H160 {
		let account_bytes: &[u8; 32] = account_id.as_ref();
		if is_eth_derived(account_id) {
			// this was originally an eth address
			// we just strip the 0xEE suffix to get the original address
			H160::from_slice(&account_bytes[..20])
		} else {
			// this is an (ed|sr)25510 derived address
			// avoid truncating the public key by hashing it first
			let account_hash = keccak_256(account_bytes);
			H160::from_slice(&account_hash[12..])
		}
	}

	fn to_account_id(address: &H160) -> AccountId32 {
		<OriginalAccount<T>>::get(address).unwrap_or_else(|| Self::to_fallback_account_id(address))
	}

	fn to_fallback_account_id(address: &H160) -> AccountId32 {
		let mut account_id = AccountId32::new([0xEE; 32]);
		let account_bytes: &mut [u8; 32] = account_id.as_mut();
		account_bytes[..20].copy_from_slice(address.as_bytes());
		account_id
	}

	fn map(account_id: &T::AccountId) -> DispatchResult {
		ensure!(!Self::is_mapped(account_id), <Error<T>>::AccountAlreadyMapped);

		// each mapping entry stores the address (20 bytes) and the account id (32 bytes)
		let deposit = T::DepositPerByte::get()
			.saturating_mul(52u32.into())
			.saturating_add(T::DepositPerItem::get());
		T::Currency::hold(&HoldReason::AddressMapping.into(), account_id, deposit)?;

		<OriginalAccount<T>>::insert(Self::to_address(account_id), account_id);
		Ok(())
	}

	/// Convenience function for benchmarking, to map an account id without taking any deposit.
	#[cfg(feature = "runtime-benchmarks")]
	fn bench_map(account_id: &T::AccountId) -> DispatchResult {
		ensure!(!Self::is_mapped(account_id), <Error<T>>::AccountAlreadyMapped);
		<OriginalAccount<T>>::insert(Self::to_address(account_id), account_id);
		Ok(())
	}

	fn unmap(account_id: &T::AccountId) -> DispatchResult {
		// will do nothing if address is not mapped so no check required
		<OriginalAccount<T>>::remove(Self::to_address(account_id));
		T::Currency::release_all(
			&HoldReason::AddressMapping.into(),
			account_id,
			Precision::BestEffort,
		)?;
		Ok(())
	}

	fn is_mapped(account_id: &T::AccountId) -> bool {
		is_eth_derived(account_id) ||
			<OriginalAccount<T>>::contains_key(Self::to_address(account_id))
	}
}

impl<T> AddressMapper<T> for TestAccountMapper<T>
where
	T: Config<AccountId = u64>,
{
	fn to_address(account_id: &T::AccountId) -> H160 {
		let mut bytes = [0u8; 20];
		bytes[12..].copy_from_slice(&account_id.to_be_bytes());
		H160::from(bytes)
	}

	fn to_account_id(address: &H160) -> T::AccountId {
		Self::to_fallback_account_id(address)
	}

	fn to_fallback_account_id(address: &H160) -> T::AccountId {
		u64::from_be_bytes(address.as_ref()[12..].try_into().unwrap())
	}

	fn map(_account_id: &T::AccountId) -> DispatchResult {
		Ok(())
	}

	fn unmap(_account_id: &T::AccountId) -> DispatchResult {
		Ok(())
	}

	fn is_mapped(_account_id: &T::AccountId) -> bool {
		true
	}
}

/// Returns true if the passed account id is controlled by an eth key.
///
/// This is a stateless check that just compares the last 12 bytes. Please note that it is
/// theoretically possible to create an ed25519 keypair that passed this filter. However,
/// this can't be used for an attack. It also won't happen by accident since everybody is using
/// sr25519 where this is not a valid public key.
pub fn is_eth_derived(account_id: &AccountId32) -> bool {
	let account_bytes: &[u8; 32] = account_id.as_ref();
	&account_bytes[20..] == &[0xEE; 12]
}

impl<T> AddressMapper<T> for H160Mapper<T>
where
	T: Config,
	crate::AccountIdOf<T>: AsRef<[u8; 20]> + From<H160>,
{
	fn to_address(account_id: &T::AccountId) -> H160 {
		H160::from_slice(account_id.as_ref())
	}

	fn to_account_id(address: &H160) -> T::AccountId {
		Self::to_fallback_account_id(address)
	}

	fn to_fallback_account_id(address: &H160) -> T::AccountId {
		(*address).into()
	}

	fn map(_account_id: &T::AccountId) -> DispatchResult {
		Ok(())
	}

	fn unmap(_account_id: &T::AccountId) -> DispatchResult {
		Ok(())
	}

	fn is_mapped(_account_id: &T::AccountId) -> bool {
		true
	}
}

/// Determine the address of a contract using CREATE semantics.
pub fn create1(deployer: &H160, nonce: u64) -> H160 {
	let mut list = rlp::RlpStream::new_list(2);
	list.append(&deployer.as_bytes());
	list.append(&nonce);
	let hash = keccak_256(&list.out());
	H160::from_slice(&hash[12..])
}

/// Determine the address of a contract using the CREATE2 semantics.
pub fn create2(deployer: &H160, code: &[u8], input_data: &[u8], salt: &[u8; 32]) -> H160 {
	let init_code_hash = {
		let init_code: Vec<u8> = code.into_iter().chain(input_data).cloned().collect();
		keccak_256(init_code.as_ref())
	};
	let mut bytes = [0; 85];
	bytes[0] = 0xff;
	bytes[1..21].copy_from_slice(deployer.as_bytes());
	bytes[21..53].copy_from_slice(salt);
	bytes[53..85].copy_from_slice(&init_code_hash);
	let hash = keccak_256(&bytes);
	H160::from_slice(&hash[12..])
}

#[cfg(test)]
mod test {
	use super::*;
	use crate::{
		test_utils::*,
		tests::{ExtBuilder, Test},
		AddressMapper, Error,
	};
	use frame_support::{
		assert_err,
		traits::fungible::{InspectHold, Mutate},
	};
	use pretty_assertions::assert_eq;
	use sp_core::{hex2array, H160};

	#[test]
	fn create1_works() {
		assert_eq!(
			create1(&ALICE_ADDR, 1u64),
			H160(hex2array!("c851da37e4e8d3a20d8d56be2963934b4ad71c3b")),
		)
	}

	#[test]
	fn create2_works() {
		assert_eq!(
			create2(
				&ALICE_ADDR,
				&hex2array!("600060005560016000"),
				&hex2array!("55"),
				&hex2array!("1234567890123456789012345678901234567890123456789012345678901234")
			),
			H160(hex2array!("7f31e795e5836a19a8f919ab5a9de9a197ecd2b6")),
		)
	}

	#[test]
	fn fallback_map_works() {
		assert!(<Test as Config>::AddressMapper::is_mapped(&ALICE));
		assert_eq!(
			ALICE_FALLBACK,
			<Test as Config>::AddressMapper::to_fallback_account_id(&ALICE_ADDR)
		);
		assert_eq!(ALICE_ADDR, <Test as Config>::AddressMapper::to_address(&ALICE_FALLBACK));
	}

	#[test]
	fn map_works() {
		ExtBuilder::default().build().execute_with(|| {
			<Test as Config>::Currency::set_balance(&EVE, 1_000_000);
			// before mapping the fallback account is returned
			assert!(!<Test as Config>::AddressMapper::is_mapped(&EVE));
			assert_eq!(EVE_FALLBACK, <Test as Config>::AddressMapper::to_account_id(&EVE_ADDR));
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&EVE
				),
				0
			);

			// when mapped the full account id is returned
			<Test as Config>::AddressMapper::map(&EVE).unwrap();
			assert!(<Test as Config>::AddressMapper::is_mapped(&EVE));
			assert_eq!(EVE, <Test as Config>::AddressMapper::to_account_id(&EVE_ADDR));
			assert!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&EVE
				) > 0
			);
		});
	}

	#[test]
	fn map_fallback_account_fails() {
		ExtBuilder::default().build().execute_with(|| {
			assert!(<Test as Config>::AddressMapper::is_mapped(&ALICE));
			// alice is an e suffixed account and hence cannot be mapped
			assert_err!(
				<Test as Config>::AddressMapper::map(&ALICE),
				<Error<Test>>::AccountAlreadyMapped,
			);
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&ALICE
				),
				0
			);
		});
	}

	#[test]
	fn double_map_fails() {
		ExtBuilder::default().build().execute_with(|| {
			assert!(!<Test as Config>::AddressMapper::is_mapped(&EVE));
			<Test as Config>::Currency::set_balance(&EVE, 1_000_000);
			<Test as Config>::AddressMapper::map(&EVE).unwrap();
			assert!(<Test as Config>::AddressMapper::is_mapped(&EVE));
			let deposit = <Test as Config>::Currency::balance_on_hold(
				&HoldReason::AddressMapping.into(),
				&EVE,
			);
			assert_err!(
				<Test as Config>::AddressMapper::map(&EVE),
				<Error<Test>>::AccountAlreadyMapped,
			);
			assert!(<Test as Config>::AddressMapper::is_mapped(&EVE));
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&EVE
				),
				deposit
			);
		});
	}

	#[test]
	fn unmap_works() {
		ExtBuilder::default().build().execute_with(|| {
			<Test as Config>::Currency::set_balance(&EVE, 1_000_000);
			<Test as Config>::AddressMapper::map(&EVE).unwrap();
			assert!(<Test as Config>::AddressMapper::is_mapped(&EVE));
			assert!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&EVE
				) > 0
			);

			<Test as Config>::AddressMapper::unmap(&EVE).unwrap();
			assert!(!<Test as Config>::AddressMapper::is_mapped(&EVE));
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&EVE
				),
				0
			);

			// another unmap is a noop
			<Test as Config>::AddressMapper::unmap(&EVE).unwrap();
			assert!(!<Test as Config>::AddressMapper::is_mapped(&EVE));
			assert_eq!(
				<Test as Config>::Currency::balance_on_hold(
					&HoldReason::AddressMapping.into(),
					&EVE
				),
				0
			);
		});
	}
}
