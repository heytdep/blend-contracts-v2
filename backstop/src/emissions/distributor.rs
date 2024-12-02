//! Methods for distributing backstop emissions to depositors

use cast::i128;
use soroban_fixed_point_math::FixedPoint;
use soroban_sdk::{unwrap::UnwrapOptimized, Address, Env};

use crate::{
    backstop::{PoolBalance, UserBalance},
    constants::SCALAR_7,
    require_nonnegative,
    storage::{self, BackstopEmissionsData, UserEmissionData},
    BackstopEmissionConfig,
};

/// Update the backstop emissions index for the user and pool
pub fn update_emissions(
    e: &Env,
    pool_id: &Address,
    pool_balance: &PoolBalance,
    user_id: &Address,
    user_balance: &UserBalance,
) {
    if let Some(emis_data) = update_emission_data(e, pool_id, pool_balance) {
        update_user_emissions(e, pool_id, user_id, &emis_data, user_balance, false);
    }
}

/// Update for claiming emissions for a user and pool
///
/// DOES NOT SEND CLAIMED TOKENS TO THE USER. The caller
/// is expected to handle sending the tokens once all claimed pools
/// have been processed.
///
/// Returns the number of tokens that need to be transferred to `user`
pub(super) fn claim_emissions(
    e: &Env,
    pool_id: &Address,
    pool_balance: &PoolBalance,
    user_id: &Address,
    user_balance: &UserBalance,
) -> i128 {
    if let Some(emis_data) = update_emission_data(e, pool_id, pool_balance) {
        update_user_emissions(e, pool_id, user_id, &emis_data, user_balance, true)
    } else {
        0
    }
}

/// Update the backstop emissions index for deposits
fn update_emission_data(
    e: &Env,
    pool_id: &Address,
    pool_balance: &PoolBalance,
) -> Option<BackstopEmissionsData> {
    match storage::get_backstop_emis_config(e, pool_id) {
        Some(config) => Some(update_emission_data_with_config(
            e,
            pool_id,
            pool_balance,
            &config,
        )),
        None => return None, // no emission exist, no update is required
    }
}

/// Update the backstop emissions index for deposits with the config already read
///
/// Stores the new backstop emissions data to the ledger
///
/// ### Returns
/// The new backstop emissions data
pub fn update_emission_data_with_config(
    e: &Env,
    pool_id: &Address,
    pool_balance: &PoolBalance,
    emis_config: &BackstopEmissionConfig,
) -> BackstopEmissionsData {
    let emis_data = storage::get_backstop_emis_data(e, pool_id).unwrap_optimized(); // exists if config is written to

    if emis_data.last_time >= emis_config.expiration
        || e.ledger().timestamp() == emis_data.last_time
        || emis_config.eps == 0
        || pool_balance.shares == 0
    {
        // emis_data already updated or expired
        return emis_data;
    }

    let max_timestamp = if e.ledger().timestamp() > emis_config.expiration {
        emis_config.expiration
    } else {
        e.ledger().timestamp()
    };

    let unqueued_shares = pool_balance.shares - pool_balance.q4w;
    require_nonnegative(e, unqueued_shares);
    let additional_idx = (i128(max_timestamp - emis_data.last_time) * i128(emis_config.eps))
        .fixed_div_floor(unqueued_shares, SCALAR_7)
        .unwrap_optimized();
    let new_data = BackstopEmissionsData {
        index: additional_idx + emis_data.index,
        last_time: e.ledger().timestamp(),
    };
    storage::set_backstop_emis_data(e, pool_id, &new_data);
    new_data
}

/// Update the user's emissions. If `to_claim` is true, the user's accrued emissions will be returned and
/// a value of zero will be stored to the ledger.
///
/// ### Returns
/// The number of emitted tokens the caller needs to send to the user
fn update_user_emissions(
    e: &Env,
    pool: &Address,
    user: &Address,
    emis_data: &BackstopEmissionsData,
    user_balance: &UserBalance,
    to_claim: bool,
) -> i128 {
    if let Some(user_data) = storage::get_user_emis_data(e, pool, user) {
        if user_data.index != emis_data.index || to_claim {
            let mut accrual = user_data.accrued;
            if user_balance.shares != 0 {
                let delta_index = emis_data.index - user_data.index;
                require_nonnegative(e, delta_index);
                let to_accrue = (user_balance.shares)
                    .fixed_mul_floor(delta_index, SCALAR_7)
                    .unwrap_optimized();
                accrual += to_accrue;
            }
            return set_user_emissions(e, pool, user, emis_data.index, accrual, to_claim);
        }
        // no accrual occured and no claim requested
        return 0;
    } else if user_balance.shares == 0 {
        // first time the user registered an action with the asset since emissions were added
        return set_user_emissions(e, pool, user, emis_data.index, 0, to_claim);
    } else {
        // user had tokens before emissions began, they are due any historical emissions
        let to_accrue = user_balance
            .shares
            .fixed_mul_floor(emis_data.index, SCALAR_7)
            .unwrap_optimized();
        return set_user_emissions(e, pool, user, emis_data.index, to_accrue, to_claim);
    }
}

fn set_user_emissions(
    e: &Env,
    pool_id: &Address,
    user: &Address,
    index: i128,
    accrued: i128,
    to_claim: bool,
) -> i128 {
    if to_claim {
        storage::set_user_emis_data(e, pool_id, user, &UserEmissionData { index, accrued: 0 });
        accrued
    } else {
        storage::set_user_emis_data(e, pool_id, user, &UserEmissionData { index, accrued });
        0
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        constants::BACKSTOP_EPOCH, storage::BackstopEmissionConfig, testutils::create_backstop, Q4W,
    };

    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger, LedgerInfo},
        vec,
    };

    /********** update_emissions **********/

    #[test]
    fn test_update_emissions() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_1000000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: BACKSTOP_EPOCH,
        };
        let user_emissions_data = UserEmissionData {
            index: 11111,
            accrued: 3,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);
            storage::set_user_emis_data(&e, &pool_1, &samwise, &user_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 9_0000000,
                q4w: vec![&e],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1).unwrap_optimized();
            let new_user_data =
                storage::get_user_emis_data(&e, &pool_1, &samwise).unwrap_optimized();
            assert_eq!(new_backstop_data.last_time, block_timestamp);
            assert_eq!(new_backstop_data.index, 8248888);
            assert_eq!(new_user_data.accrued, 7_4139996);
            assert_eq!(new_user_data.index, 8248888);
        });
    }

    #[test]
    fn test_update_emissions_no_config() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 9_0000000,
                q4w: vec![&e],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1);
            let new_user_data = storage::get_user_emis_data(&e, &pool_1, &samwise);
            assert!(new_backstop_data.is_none());
            assert!(new_user_data.is_none());
        });
    }

    #[test]
    fn test_update_emissions_first_action() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 12345;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_0420000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: BACKSTOP_EPOCH,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 0,
                q4w: vec![&e],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1).unwrap_optimized();
            let new_user_data =
                storage::get_user_emis_data(&e, &pool_1, &samwise).unwrap_optimized();
            assert_eq!(new_backstop_data.last_time, block_timestamp);
            assert_eq!(new_backstop_data.index, 34588222);
            assert_eq!(new_user_data.accrued, 0);
            assert_eq!(new_user_data.index, 34588222);
        });
    }

    #[test]
    fn test_update_emissions_config_set_after_user() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 12345;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_0420000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 0,
            last_time: BACKSTOP_EPOCH,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 9_0000000,
                q4w: vec![&e],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1).unwrap_optimized();
            let new_user_data =
                storage::get_user_emis_data(&e, &pool_1, &samwise).unwrap_optimized();
            assert_eq!(new_backstop_data.last_time, block_timestamp);
            assert_eq!(new_backstop_data.index, 34566000);
            assert_eq!(new_user_data.accrued, 31_1094000);
            assert_eq!(new_user_data.index, 34566000);
        });
    }

    #[test]
    fn test_update_emissions_q4w_not_counted() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_1000000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: BACKSTOP_EPOCH,
        };
        let user_emissions_data = UserEmissionData {
            index: 11111,
            accrued: 3,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);
            storage::set_user_emis_data(&e, &pool_1, &samwise, &user_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 4_5000000,
            };
            let q4w: Q4W = Q4W {
                amount: (4_5000000),
                exp: (5000),
            };
            let user_balance = UserBalance {
                shares: 4_5000000,
                q4w: vec![&e, q4w],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1).unwrap_optimized();
            let new_user_data =
                storage::get_user_emis_data(&e, &pool_1, &samwise).unwrap_optimized();
            assert_eq!(new_backstop_data.last_time, block_timestamp);
            assert_eq!(new_backstop_data.index, 8503321);
            assert_eq!(new_user_data.accrued, 38214948);
            assert_eq!(new_user_data.index, 8503321);
        });
    }

    #[test]
    fn test_claim_emissions() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_1000000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: BACKSTOP_EPOCH,
        };
        let user_emissions_data = UserEmissionData {
            index: 11111,
            accrued: 3,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);
            storage::set_user_emis_data(&e, &pool_1, &samwise, &user_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 9_0000000,
                q4w: vec![&e],
            };

            let result = claim_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1).unwrap_optimized();
            let new_user_data =
                storage::get_user_emis_data(&e, &pool_1, &samwise).unwrap_optimized();
            assert_eq!(result, 7_4139996);
            assert_eq!(new_backstop_data.last_time, block_timestamp);
            assert_eq!(new_backstop_data.index, 8248888);
            assert_eq!(new_user_data.accrued, 0);
            assert_eq!(new_user_data.index, 8248888);
        });
    }

    #[test]
    fn test_claim_emissions_no_config() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 9_0000000,
                q4w: vec![&e],
            };

            let result = claim_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);

            assert_eq!(result, 0);
            let new_backstop_data = storage::get_backstop_emis_data(&e, &pool_1);
            let new_user_data = storage::get_user_emis_data(&e, &pool_1, &samwise);
            assert!(new_backstop_data.is_none());
            assert!(new_user_data.is_none());
        });
    }

    // @dev: The below tests should be impossible states to reach, but are left
    //       in to ensure any bad state does not result in incorrect emissions.

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn test_update_emissions_more_q4w_than_shares_panics() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_1000000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: BACKSTOP_EPOCH,
        };
        let user_emissions_data = UserEmissionData {
            index: 11111,
            accrued: 3,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);
            storage::set_user_emis_data(&e, &pool_1, &samwise, &user_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 150_0000001,
            };
            let q4w: Q4W = Q4W {
                amount: (4_5000000),
                exp: (5000),
            };
            let user_balance = UserBalance {
                shares: 4_5000000,
                q4w: vec![&e, q4w],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);
        });
    }

    #[test]
    #[should_panic(expected = "attempt to subtract with overflow")]
    fn test_update_emissions_negative_time_dif() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_1000000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: block_timestamp + 1,
        };
        let user_emissions_data = UserEmissionData {
            index: 11111,
            accrued: 3,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);
            storage::set_user_emis_data(&e, &pool_1, &samwise, &user_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 4_5000000,
                q4w: vec![&e],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);
        });
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #8)")]
    fn test_update_emissions_negative_user_index() {
        let e = Env::default();
        let block_timestamp = BACKSTOP_EPOCH + 1234;
        e.ledger().set(LedgerInfo {
            timestamp: block_timestamp,
            protocol_version: 21,
            sequence_number: 0,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let backstop_id = create_backstop(&e);
        let pool_1 = Address::generate(&e);
        let samwise = Address::generate(&e);

        let backstop_emissions_config = BackstopEmissionConfig {
            expiration: BACKSTOP_EPOCH + 7 * 24 * 60 * 60,
            eps: 0_1000000,
        };
        let backstop_emissions_data = BackstopEmissionsData {
            index: 22222,
            last_time: BACKSTOP_EPOCH,
        };
        let user_emissions_data = UserEmissionData {
            index: 34566000 + 1,
            accrued: 3,
        };
        e.as_contract(&backstop_id, || {
            storage::set_last_distribution_time(&e, &BACKSTOP_EPOCH);
            storage::set_backstop_emis_config(&e, &pool_1, &backstop_emissions_config);
            storage::set_backstop_emis_data(&e, &pool_1, &backstop_emissions_data);
            storage::set_user_emis_data(&e, &pool_1, &samwise, &user_emissions_data);

            let pool_balance = PoolBalance {
                shares: 150_0000000,
                tokens: 200_0000000,
                q4w: 0,
            };
            let user_balance = UserBalance {
                shares: 4_5000000,
                q4w: vec![&e],
            };

            update_emissions(&e, &pool_1, &pool_balance, &samwise, &user_balance);
        });
    }
}
