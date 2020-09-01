//! Functions for accessing and mutating local and global state.

use alloc::{collections::BTreeSet, string::String, vec, vec::Vec};
use core::{convert::From, mem::MaybeUninit};

use casper_types::{
    api_error,
    bytesrepr::{self, FromBytes, ToBytes},
    contracts::{ContractVersion, EntryPoints, NamedKeys},
    AccessRights, ApiError, CLTyped, CLValue, ContractHash, ContractPackageHash, Key, URef,
    UREF_SERIALIZED_LENGTH,
};

use crate::{
    contract_api::{self, runtime, runtime::revert},
    ext_ffi,
    unwrap_or_revert::UnwrapOrRevert,
};

/// Reads value under `uref` in the global state.
pub fn read<T: CLTyped + FromBytes>(uref: URef) -> Result<Option<T>, bytesrepr::Error> {
    let key: Key = uref.into();
    let (key_ptr, key_size, _bytes) = contract_api::to_ptr(key);

    let value_size = {
        let mut value_size = MaybeUninit::uninit();
        let ret = unsafe { ext_ffi::read_value(key_ptr, key_size, value_size.as_mut_ptr()) };
        match api_error::result_from(ret) {
            Ok(_) => unsafe { value_size.assume_init() },
            Err(ApiError::ValueNotFound) => return Ok(None),
            Err(e) => runtime::revert(e),
        }
    };

    let value_bytes = runtime::read_host_buffer(value_size).unwrap_or_revert();
    Ok(Some(bytesrepr::deserialize(value_bytes)?))
}

/// Reads value under `uref` in the global state, reverts if value not found or is not `T`.
pub fn read_or_revert<T: CLTyped + FromBytes>(uref: URef) -> T {
    read(uref)
        .unwrap_or_revert_with(ApiError::Read)
        .unwrap_or_revert_with(ApiError::ValueNotFound)
}

/// Reads the value under `key` in the context-local partition of global state.
pub fn read_local<K: ToBytes, V: CLTyped + FromBytes>(
    key: &K,
) -> Result<Option<V>, bytesrepr::Error> {
    let key_bytes = key.to_bytes()?;

    let value_size = {
        let mut value_size = MaybeUninit::uninit();
        let ret = unsafe {
            ext_ffi::read_value_local(key_bytes.as_ptr(), key_bytes.len(), value_size.as_mut_ptr())
        };
        match api_error::result_from(ret) {
            Ok(_) => unsafe { value_size.assume_init() },
            Err(ApiError::ValueNotFound) => return Ok(None),
            Err(e) => runtime::revert(e),
        }
    };

    let value_bytes = runtime::read_host_buffer(value_size).unwrap_or_revert();
    Ok(Some(bytesrepr::deserialize(value_bytes)?))
}

/// Writes `value` under `uref` in the global state.
pub fn write<T: CLTyped + ToBytes>(uref: URef, value: T) {
    let key = Key::from(uref);
    let (key_ptr, key_size, _bytes1) = contract_api::to_ptr(key);

    let cl_value = CLValue::from_t(value).unwrap_or_revert();
    let (cl_value_ptr, cl_value_size, _bytes2) = contract_api::to_ptr(cl_value);

    unsafe {
        ext_ffi::write(key_ptr, key_size, cl_value_ptr, cl_value_size);
    }
}

/// Writes `value` under `key` in the context-local partition of global state.
pub fn write_local<K: ToBytes, V: CLTyped + ToBytes>(key: K, value: V) {
    let (key_ptr, key_size, _bytes1) = contract_api::to_ptr(key);

    let cl_value = CLValue::from_t(value).unwrap_or_revert();
    let (cl_value_ptr, cl_value_size, _bytes) = contract_api::to_ptr(cl_value);

    unsafe {
        ext_ffi::write_local(key_ptr, key_size, cl_value_ptr, cl_value_size);
    }
}

/// Adds `value` to the one currently under `uref` in the global state.
pub fn add<T: CLTyped + ToBytes>(uref: URef, value: T) {
    let key = Key::from(uref);
    let (key_ptr, key_size, _bytes1) = contract_api::to_ptr(key);

    let cl_value = CLValue::from_t(value).unwrap_or_revert();
    let (cl_value_ptr, cl_value_size, _bytes2) = contract_api::to_ptr(cl_value);

    unsafe {
        // Could panic if `value` cannot be added to the given value in memory.
        ext_ffi::add(key_ptr, key_size, cl_value_ptr, cl_value_size);
    }
}

/// Adds `value` to the one currently under `key` in the context-local partition of global state.
pub fn add_local<K: ToBytes, V: CLTyped + ToBytes>(key: K, value: V) {
    let (key_ptr, key_size, _bytes1) = contract_api::to_ptr(key);

    let cl_value = CLValue::from_t(value).unwrap_or_revert();
    let (cl_value_ptr, cl_value_size, _bytes) = contract_api::to_ptr(cl_value);

    unsafe {
        ext_ffi::add_local(key_ptr, key_size, cl_value_ptr, cl_value_size);
    }
}

/// Returns a new unforgeable pointer, where the value is initialized to `init`.
pub fn new_uref<T: CLTyped + ToBytes>(init: T) -> URef {
    let uref_non_null_ptr = contract_api::alloc_bytes(UREF_SERIALIZED_LENGTH);
    let cl_value = CLValue::from_t(init).unwrap_or_revert();
    let (cl_value_ptr, cl_value_size, _cl_value_bytes) = contract_api::to_ptr(cl_value);
    let bytes = unsafe {
        ext_ffi::new_uref(uref_non_null_ptr.as_ptr(), cl_value_ptr, cl_value_size); // URef has `READ_ADD_WRITE`
        Vec::from_raw_parts(
            uref_non_null_ptr.as_ptr(),
            UREF_SERIALIZED_LENGTH,
            UREF_SERIALIZED_LENGTH,
        )
    };
    bytesrepr::deserialize(bytes).unwrap_or_revert()
}

/// Create a new contract stored under a Key::Hash at version 1
/// if `named_keys` are provided, will apply them
/// if `hash_name` is provided, puts contract hash in current context's named keys under `hash_name`
/// if `uref_name` is provided, puts access_uref in current context's named keys under `uref_name`
pub fn new_contract(
    entry_points: EntryPoints,
    named_keys: Option<NamedKeys>,
    hash_name: Option<String>,
    uref_name: Option<String>,
) -> (ContractHash, ContractVersion) {
    let (contract_package_hash, access_uref) = create_contract_package_at_hash();

    if let Some(hash_name) = hash_name {
        runtime::put_key(&hash_name, contract_package_hash.into());
    };

    if let Some(uref_name) = uref_name {
        runtime::put_key(&uref_name, access_uref.into());
    };

    let named_keys = match named_keys {
        Some(named_keys) => named_keys,
        None => NamedKeys::new(),
    };

    add_contract_version(contract_package_hash, entry_points, named_keys)
}

/// Create a new (versioned) contract stored under a Key::Hash. Initially there
/// are no versions; a version must be added via `add_contract_version` before
/// the contract can be executed.
pub fn create_contract_package_at_hash() -> (ContractPackageHash, URef) {
    let mut hash_addr = ContractPackageHash::default();
    let mut access_addr = [0u8; 32];
    unsafe {
        ext_ffi::create_contract_package_at_hash(hash_addr.as_mut_ptr(), access_addr.as_mut_ptr());
    }
    let contract_package_hash = hash_addr;
    let access_uref = URef::new(access_addr, AccessRights::READ_ADD_WRITE);

    (contract_package_hash, access_uref)
}

/// Create a new "user group" for a (versioned) contract. User groups associate
/// a set of URefs with a label. Entry points on a contract can be given a list of
/// labels they accept and the runtime will check that a URef from at least one
/// of the allowed groups is present in the caller's context before
/// execution. This allows access control for entry_points of a contract. This
/// function returns the list of new URefs created for the group (the list will
/// contain `num_new_urefs` elements).
pub fn create_contract_user_group(
    contract_package_hash: ContractPackageHash,
    group_label: &str,
    num_new_urefs: u8, // number of new urefs to populate the group with
    existing_urefs: BTreeSet<URef>, // also include these existing urefs in the group
) -> Result<Vec<URef>, ApiError> {
    let (contract_package_hash_ptr, contract_package_hash_size, _bytes1) =
        contract_api::to_ptr(contract_package_hash);
    let (label_ptr, label_size, _bytes3) = contract_api::to_ptr(group_label);
    let (existing_urefs_ptr, existing_urefs_size, _bytes4) = contract_api::to_ptr(existing_urefs);

    let value_size = {
        let mut output_size = MaybeUninit::uninit();
        let ret = unsafe {
            ext_ffi::create_contract_user_group(
                contract_package_hash_ptr,
                contract_package_hash_size,
                label_ptr,
                label_size,
                num_new_urefs,
                existing_urefs_ptr,
                existing_urefs_size,
                output_size.as_mut_ptr(),
            )
        };
        api_error::result_from(ret).unwrap_or_revert();
        unsafe { output_size.assume_init() }
    };

    let value_bytes = runtime::read_host_buffer(value_size).unwrap_or_revert();
    Ok(bytesrepr::deserialize(value_bytes).unwrap_or_revert())
}

/// Extends specified group with a new `URef`.
pub fn provision_contract_user_group_uref(
    package_hash: ContractPackageHash,
    label: &str,
) -> Result<URef, ApiError> {
    let (contract_package_hash_ptr, contract_package_hash_size, _bytes1) =
        contract_api::to_ptr(package_hash);
    let (label_ptr, label_size, _bytes2) = contract_api::to_ptr(label);
    let value_size = {
        let mut value_size = MaybeUninit::uninit();
        let ret = unsafe {
            ext_ffi::provision_contract_user_group_uref(
                contract_package_hash_ptr,
                contract_package_hash_size,
                label_ptr,
                label_size,
                value_size.as_mut_ptr(),
            )
        };
        api_error::result_from(ret)?;
        unsafe { value_size.assume_init() }
    };
    let value_bytes = runtime::read_host_buffer(value_size).unwrap_or_revert();
    Ok(bytesrepr::deserialize(value_bytes).unwrap_or_revert())
}

/// Removes specified urefs from a named group.
pub fn remove_contract_user_group_urefs(
    package_hash: ContractPackageHash,
    label: &str,
    urefs: BTreeSet<URef>,
) -> Result<(), ApiError> {
    let (contract_package_hash_ptr, contract_package_hash_size, _bytes1) =
        contract_api::to_ptr(package_hash);
    let (label_ptr, label_size, _bytes3) = contract_api::to_ptr(label);
    let (urefs_ptr, urefs_size, _bytes4) = contract_api::to_ptr(urefs);
    let ret = unsafe {
        ext_ffi::remove_contract_user_group_urefs(
            contract_package_hash_ptr,
            contract_package_hash_size,
            label_ptr,
            label_size,
            urefs_ptr,
            urefs_size,
        )
    };
    api_error::result_from(ret)
}

/// Remove a named group from given contract.
pub fn remove_contract_user_group(
    package_hash: ContractPackageHash,
    label: &str,
) -> Result<(), ApiError> {
    let (contract_package_hash_ptr, contract_package_hash_size, _bytes1) =
        contract_api::to_ptr(package_hash);
    let (label_ptr, label_size, _bytes3) = contract_api::to_ptr(label);
    let ret = unsafe {
        ext_ffi::remove_contract_user_group(
            contract_package_hash_ptr,
            contract_package_hash_size,
            label_ptr,
            label_size,
        )
    };
    api_error::result_from(ret)
}

/// Add a new version of a contract to the contract stored at the given
/// `Key`. Note that this contract must have been created by
/// `create_contract` or `create_contract_package_at_hash` first.
pub fn add_contract_version(
    contract_package_hash: ContractPackageHash,
    entry_points: EntryPoints,
    named_keys: NamedKeys,
) -> (ContractHash, ContractVersion) {
    let (contract_package_hash_ptr, contract_package_hash_size, _bytes1) =
        contract_api::to_ptr(contract_package_hash);
    let (entry_points_ptr, entry_points_size, _bytes4) = contract_api::to_ptr(entry_points);
    let (named_keys_ptr, named_keys_size, _bytes5) = contract_api::to_ptr(named_keys);

    let mut output_ptr = vec![0u8; Key::max_serialized_length()];
    let mut total_bytes: usize = 0;

    let mut contract_version: ContractVersion = 0;

    let ret = unsafe {
        ext_ffi::add_contract_version(
            contract_package_hash_ptr,
            contract_package_hash_size,
            &mut contract_version as *mut ContractVersion,
            entry_points_ptr,
            entry_points_size,
            named_keys_ptr,
            named_keys_size,
            output_ptr.as_mut_ptr(),
            output_ptr.len(),
            &mut total_bytes as *mut usize,
        )
    };
    match api_error::result_from(ret) {
        Ok(_) => {}
        Err(e) => revert(e),
    }
    output_ptr.truncate(total_bytes);
    let contract_hash = bytesrepr::deserialize(output_ptr).unwrap_or_revert();
    (contract_hash, contract_version)
}

/// Disable a version of a contract from the contract stored at the given
/// `Key`. That version of the contract will no longer be callable by
/// `call_versioned_contract`. Note that this contract must have been created by
/// `create_contract` or `create_contract_package_at_hash` first.
pub fn disable_contract_version(
    contract_package_hash: ContractPackageHash,
    contract_hash: ContractHash,
) -> Result<(), ApiError> {
    let (contract_package_hash_ptr, contract_package_hash_size, _bytes1) =
        contract_api::to_ptr(contract_package_hash);
    let (contract_hash_ptr, contract_hash_size, _bytes2) = contract_api::to_ptr(contract_hash);

    let result = unsafe {
        ext_ffi::disable_contract_version(
            contract_package_hash_ptr,
            contract_package_hash_size,
            contract_hash_ptr,
            contract_hash_size,
        )
    };

    api_error::result_from(result)
}
