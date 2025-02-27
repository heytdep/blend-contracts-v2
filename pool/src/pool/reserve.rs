use cast::i128;
use soroban_fixed_point_math::FixedPoint;
use soroban_sdk::{contracttype, panic_with_error, unwrap::UnwrapOptimized, Address, Env};

use crate::{
    constants::{SCALAR_7, SCALAR_9},
    errors::PoolError,
    pool::actions::RequestType,
    storage::{self, PoolConfig, ReserveData},
};

use super::interest::calc_accrual;

#[derive(Clone)]
#[contracttype]
pub struct Reserve {
    pub asset: Address,        // the underlying asset address
    pub index: u32,            // the reserve index in the pool
    pub l_factor: u32,         // the liability factor for the reserve
    pub c_factor: u32,         // the collateral factor for the reserve
    pub max_util: u32,         // the maximum utilization rate for the reserve
    pub last_time: u64,        // the last block the data was updated
    pub scalar: i128,          // scalar used for positions, b/d token supply, and credit
    pub d_rate: i128,          // the conversion rate from dToken to underlying (9 decimals)
    pub b_rate: i128,          // the conversion rate from bToken to underlying (9 decimals)
    pub ir_mod: i128,          // the interest rate curve modifier (9 decimals)
    pub b_supply: i128,        // the total supply of b tokens
    pub d_supply: i128,        // the total supply of d tokens
    pub backstop_credit: i128, // the total amount of underlying tokens owed to the backstop
    pub collateral_cap: i128, // the total amount of underlying tokens that can be used as collateral
    pub enabled: bool,        // is the reserve enabled
}

impl Reserve {
    /// Load a Reserve from the ledger and update to the current ledger timestamp.
    ///
    /// **NOTE**: This function is not cached, and should be called from the Pool.
    ///
    /// ### Arguments
    /// * pool_config - The pool configuration
    /// * asset - The address of the underlying asset
    ///
    /// ### Panics
    /// Panics if the asset is not supported, if emissions cannot be updated, or if the reserve
    /// cannot be updated to the current ledger timestamp.
    pub fn load(e: &Env, pool_config: &PoolConfig, asset: &Address) -> Reserve {
        let reserve_config = storage::get_res_config(e, asset);
        let reserve_data = storage::get_res_data(e, asset);
        let mut reserve = Reserve {
            asset: asset.clone(),
            index: reserve_config.index,
            l_factor: reserve_config.l_factor,
            c_factor: reserve_config.c_factor,
            max_util: reserve_config.max_util,
            last_time: reserve_data.last_time,
            scalar: 10i128.pow(reserve_config.decimals),
            d_rate: reserve_data.d_rate,
            b_rate: reserve_data.b_rate,
            ir_mod: reserve_data.ir_mod,
            b_supply: reserve_data.b_supply,
            d_supply: reserve_data.d_supply,
            backstop_credit: reserve_data.backstop_credit,
            collateral_cap: reserve_config.collateral_cap,
            enabled: reserve_config.enabled,
        };

        // short circuit if the reserve has already been updated this ledger
        if e.ledger().timestamp() == reserve.last_time {
            return reserve;
        }

        if reserve.b_supply == 0 {
            reserve.last_time = e.ledger().timestamp();
            return reserve;
        }

        let cur_util = reserve.utilization();
        if cur_util == 0 {
            // if there are no assets borrowed, we don't need to update the reserve
            reserve.last_time = e.ledger().timestamp();
            return reserve;
        }

        let (loan_accrual, new_ir_mod) = calc_accrual(
            e,
            &reserve_config,
            cur_util,
            reserve.ir_mod,
            reserve.last_time,
        );
        reserve.ir_mod = new_ir_mod;

        let pre_update_liabilities = reserve.total_liabilities();
        reserve.d_rate = loan_accrual
            .fixed_mul_ceil(reserve.d_rate, SCALAR_9)
            .unwrap_optimized();
        let accrued_interest = reserve.total_liabilities() - pre_update_liabilities;

        reserve.gulp(pool_config.bstop_rate, accrued_interest);

        reserve.last_time = e.ledger().timestamp();
        reserve
    }

    /// Store the updated reserve to the ledger.
    pub fn store(&self, e: &Env) {
        let reserve_data = ReserveData {
            d_rate: self.d_rate,
            b_rate: self.b_rate,
            ir_mod: self.ir_mod,
            b_supply: self.b_supply,
            d_supply: self.d_supply,
            backstop_credit: self.backstop_credit,
            last_time: self.last_time,
        };
        storage::set_res_data(e, &self.asset, &reserve_data);
    }

    /// Accrue tokens to the reserve supply. This issues any `backstop_credit` required and updates the reserve's bRate to account for the additional tokens.
    ///
    /// ### Arguments
    /// * bstop_rate - The backstop take rate for the pool
    /// * accrued - The amount of additional underlying tokens
    pub fn gulp(&mut self, bstop_rate: u32, accrued: i128) {
        let pre_update_supply = self.total_supply();

        if accrued > 0 {
            // credit the backstop underlying from the accrued interest based on the backstop rate
            // update the accrued interest to reflect the amount the pool accrued
            let mut new_backstop_credit: i128 = 0;
            if bstop_rate > 0 {
                new_backstop_credit = accrued
                    .fixed_mul_floor(i128(bstop_rate), SCALAR_7)
                    .unwrap_optimized();
                self.backstop_credit += new_backstop_credit;
            }
            self.b_rate = (pre_update_supply + accrued - new_backstop_credit)
                .fixed_div_floor(self.b_supply, SCALAR_9)
                .unwrap_optimized();
        }
    }

    /// Fetch the current utilization rate for the reserve normalized to 7 decimals
    pub fn utilization(&self) -> i128 {
        self.total_liabilities()
            .fixed_div_ceil(self.total_supply(), SCALAR_7)
            .unwrap_optimized()
    }

    /// Require that the utilization rate is below the maximum allowed, or panic.
    pub fn require_utilization_below_max(&self, e: &Env) {
        if self.utilization() > i128(self.max_util) {
            panic_with_error!(e, PoolError::InvalidUtilRate)
        }
    }

    /// Check the action is allowed according to the reserve status, or panic.
    ///
    /// ### Arguments
    /// * `action_type` - The type of action being performed
    pub fn require_action_allowed(&self, e: &Env, action_type: u32) {
        // disable borrowing or auction cancellation for any non-active pool and disable supplying for any frozen pool
        if !self.enabled {
            if action_type == RequestType::Supply as u32
                || action_type == RequestType::SupplyCollateral as u32
                || action_type == RequestType::Borrow as u32
            {
                panic_with_error!(e, PoolError::ReserveDisabled);
            }
        }
    }

    /// Fetch the total liabilities for the reserve in underlying tokens
    pub fn total_liabilities(&self) -> i128 {
        self.to_asset_from_d_token(self.d_supply)
    }

    /// Fetch the total supply for the reserve in underlying tokens
    pub fn total_supply(&self) -> i128 {
        self.to_asset_from_b_token(self.b_supply)
    }

    /********** Conversion Functions **********/

    /// Convert d_tokens to the corresponding asset value
    ///
    /// ### Arguments
    /// * `d_tokens` - The amount of tokens to convert
    pub fn to_asset_from_d_token(&self, d_tokens: i128) -> i128 {
        d_tokens
            .fixed_mul_ceil(self.d_rate, SCALAR_9)
            .unwrap_optimized()
    }

    /// Convert b_tokens to the corresponding asset value
    ///
    /// ### Arguments
    /// * `b_tokens` - The amount of tokens to convert
    pub fn to_asset_from_b_token(&self, b_tokens: i128) -> i128 {
        b_tokens
            .fixed_mul_floor(self.b_rate, SCALAR_9)
            .unwrap_optimized()
    }

    /// Convert d_tokens to their corresponding effective asset value. This
    /// takes into account the liability factor.
    ///
    /// ### Arguments
    /// * `d_tokens` - The amount of tokens to convert
    pub fn to_effective_asset_from_d_token(&self, d_tokens: i128) -> i128 {
        let assets = self.to_asset_from_d_token(d_tokens);
        assets
            .fixed_div_ceil(i128(self.l_factor), SCALAR_7)
            .unwrap_optimized()
    }

    /// Convert b_tokens to the corresponding effective asset value. This
    /// takes into account the collateral factor.
    ///
    /// ### Arguments
    /// * `b_tokens` - The amount of tokens to convert
    pub fn to_effective_asset_from_b_token(&self, b_tokens: i128) -> i128 {
        let assets = self.to_asset_from_b_token(b_tokens);
        assets
            .fixed_mul_floor(i128(self.c_factor), SCALAR_7)
            .unwrap_optimized()
    }

    /// Convert asset tokens to the corresponding d token value - rounding up
    ///
    /// ### Arguments
    /// * `amount` - The amount of tokens to convert
    pub fn to_d_token_up(&self, amount: i128) -> i128 {
        amount
            .fixed_div_ceil(self.d_rate, SCALAR_9)
            .unwrap_optimized()
    }

    /// Convert asset tokens to the corresponding d token value - rounding down
    ///
    /// ### Arguments
    /// * `amount` - The amount of tokens to convert
    pub fn to_d_token_down(&self, amount: i128) -> i128 {
        amount
            .fixed_div_floor(self.d_rate, SCALAR_9)
            .unwrap_optimized()
    }

    /// Convert asset tokens to the corresponding b token value - round up
    ///
    /// ### Arguments
    /// * `amount` - The amount of tokens to convert
    pub fn to_b_token_up(&self, amount: i128) -> i128 {
        amount
            .fixed_div_ceil(self.b_rate, SCALAR_9)
            .unwrap_optimized()
    }

    /// Convert asset tokens to the corresponding b token value - round down
    ///
    /// ### Arguments
    /// * `amount` - The amount of tokens to convert
    pub fn to_b_token_down(&self, amount: i128) -> i128 {
        amount
            .fixed_div_floor(self.b_rate, SCALAR_9)
            .unwrap_optimized()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutils;
    use soroban_sdk::testutils::{Address as _, Ledger, LedgerInfo};
    #[test]
    fn test_load_reserve() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let oracle = Address::generate(&e);

        let (underlying, _) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.d_rate = 1_345_678_123;
        reserve_data.b_rate = 1_123_456_789;
        reserve_data.d_supply = 65_0000000;
        reserve_data.b_supply = 99_0000000;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let pool_config = PoolConfig {
            oracle,
            bstop_rate: 0_2000000,
            status: 0,
            max_positions: 5,
        };
        e.as_contract(&pool, || {
            storage::set_pool_config(&e, &pool_config);
            let reserve = Reserve::load(&e, &pool_config, &underlying);

            // (accrual: 1_002_957_369, util: .7864353)
            assert_eq!(reserve.d_rate, 1_349_657_800);
            assert_eq!(reserve.b_rate, 1_125_547_124);
            assert_eq!(reserve.ir_mod, 1_044_981_563);
            assert_eq!(reserve.d_supply, 65_0000000);
            assert_eq!(reserve.b_supply, 99_0000000);
            assert_eq!(reserve.backstop_credit, 0_0517358);
            assert_eq!(reserve.last_time, 617280);
        });
    }

    #[test]
    fn test_load_reserve_zero_supply() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let oracle = Address::generate(&e);

        let (underlying, _) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.d_rate = 0;
        reserve_data.b_rate = 0;
        reserve_data.d_supply = 0;
        reserve_data.b_supply = 0;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let pool_config = PoolConfig {
            oracle,
            bstop_rate: 0_2000000,
            status: 0,
            max_positions: 4,
        };
        e.as_contract(&pool, || {
            storage::set_pool_config(&e, &pool_config);
            let reserve = Reserve::load(&e, &pool_config, &underlying);

            // (accrual: 1_002_957_369, util: .7864352)q
            assert_eq!(reserve.d_rate, 0);
            assert_eq!(reserve.b_rate, 0);
            assert_eq!(reserve.ir_mod, 1_000_000_000);
            assert_eq!(reserve.d_supply, 0);
            assert_eq!(reserve.b_supply, 0);
            assert_eq!(reserve.backstop_credit, 0);
            assert_eq!(reserve.last_time, 617280);
        });
    }

    #[test]
    fn test_load_reserve_zero_util() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let oracle = Address::generate(&e);

        let (underlying, _) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.d_rate = 0;
        reserve_data.d_supply = 0;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let pool_config = PoolConfig {
            oracle,
            bstop_rate: 0_2000000,
            status: 0,
            max_positions: 4,
        };
        e.as_contract(&pool, || {
            storage::set_pool_config(&e, &pool_config);
            let reserve = Reserve::load(&e, &pool_config, &underlying);

            assert_eq!(reserve.d_rate, 0);
            assert_eq!(reserve.b_rate, reserve_data.b_rate);
            assert_eq!(reserve.ir_mod, reserve_data.ir_mod);
            assert_eq!(reserve.d_supply, 0);
            assert_eq!(reserve.b_supply, reserve_data.b_supply);
            assert_eq!(reserve.backstop_credit, 0);
            assert_eq!(reserve.last_time, 617280);
        });
    }

    #[test]
    fn test_load_reserve_zero_bstop_rate() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let oracle = Address::generate(&e);

        let (underlying, _) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.d_rate = 1_345_678_123;
        reserve_data.b_rate = 1_123_456_789;
        reserve_data.d_supply = 65_0000000;
        reserve_data.b_supply = 99_0000000;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let pool_config = PoolConfig {
            oracle,
            bstop_rate: 0,
            status: 0,
            max_positions: 4,
        };
        e.as_contract(&pool, || {
            storage::set_pool_config(&e, &pool_config);
            let reserve = Reserve::load(&e, &pool_config, &underlying);

            // (accrual: 1_002_957_369, util: .7864353)
            assert_eq!(reserve.d_rate, 1_349_657_800);
            assert_eq!(reserve.b_rate, 1_126_069_708);
            assert_eq!(reserve.ir_mod, 1_044_981_563);
            assert_eq!(reserve.d_supply, 65_0000000);
            assert_eq!(reserve.b_supply, 99_0000000);
            assert_eq!(reserve.backstop_credit, 0);
            assert_eq!(reserve.last_time, 617280);
        });
    }

    #[test]
    fn test_store() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let bombadil = Address::generate(&e);
        let pool = testutils::create_pool(&e);
        let oracle = Address::generate(&e);

        let (underlying, _) = testutils::create_token_contract(&e, &bombadil);
        let (reserve_config, mut reserve_data) = testutils::default_reserve_meta();
        reserve_data.d_rate = 1_345_678_123;
        reserve_data.b_rate = 1_123_456_789;
        reserve_data.d_supply = 65_0000000;
        reserve_data.b_supply = 99_0000000;
        testutils::create_reserve(&e, &pool, &underlying, &reserve_config, &reserve_data);

        let pool_config = PoolConfig {
            oracle,
            bstop_rate: 0_2000000,
            status: 0,
            max_positions: 4,
        };
        e.as_contract(&pool, || {
            storage::set_pool_config(&e, &pool_config);
            let reserve = Reserve::load(&e, &pool_config, &underlying);
            reserve.store(&e);

            let reserve_data = storage::get_res_data(&e, &underlying);

            // (accrual: 1_002_957_369, util: .7864353)
            assert_eq!(reserve_data.d_rate, 1_349_657_800);
            assert_eq!(reserve_data.b_rate, 1_125_547_124);
            assert_eq!(reserve_data.ir_mod, 1_044_981_563);
            assert_eq!(reserve_data.d_supply, 65_0000000);
            assert_eq!(reserve_data.b_supply, 99_0000000);
            assert_eq!(reserve_data.backstop_credit, 0_0517358);
            assert_eq!(reserve_data.last_time, 617280);
        });
    }

    #[test]
    fn test_utilization() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.d_rate = 1_345_678_123;
        reserve.b_rate = 1_123_456_789;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.utilization();

        assert_eq!(result, 0_7864353);
    }

    #[test]
    fn test_require_utilization_below_max_pass() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        reserve.require_utilization_below_max(&e);
        // no panic
        assert!(true);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1207)")]
    fn test_require_utilization_under_max_panic() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_supply = 100_0000000;
        reserve.d_supply = 95_0000100;

        reserve.require_utilization_below_max(&e);
    }

    /***** Token Transfer Math *****/

    #[test]
    fn test_to_asset_from_d_token() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.d_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.to_asset_from_d_token(1_1234567);

        assert_eq!(result, 1_4850244);
    }

    #[test]
    fn test_to_asset_from_b_token() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.to_asset_from_b_token(1_1234567);

        assert_eq!(result, 1_4850243);
    }

    #[test]
    fn test_to_effective_asset_from_d_token() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.d_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;
        reserve.l_factor = 1_1000000;

        let result = reserve.to_effective_asset_from_d_token(1_1234567);

        assert_eq!(result, 1_3500222);
    }

    #[test]
    fn test_to_effective_asset_from_b_token() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;
        reserve.c_factor = 0_8500000;

        let result = reserve.to_effective_asset_from_b_token(1_1234567);

        assert_eq!(result, 1_2622706);
    }

    #[test]
    fn test_total_liabilities() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.d_rate = 1_823_912_692;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.total_liabilities();

        assert_eq!(result, 118_5543250);
    }

    #[test]
    fn test_total_supply() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_rate = 1_823_912_692;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.total_supply();

        assert_eq!(result, 180_5673565);
    }

    #[test]
    fn test_to_d_token_up() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.d_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.to_d_token_up(1_4850243);

        assert_eq!(result, 1_1234567);
    }

    #[test]
    fn test_to_d_token_down() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.d_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.to_d_token_down(1_4850243);

        assert_eq!(result, 1_1234566);
    }

    #[test]
    fn test_to_b_token_up() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.to_b_token_up(1_4850243);

        assert_eq!(result, 1_1234567);
    }

    #[test]
    fn test_to_b_token_down() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.b_rate = 1_321_834_961;
        reserve.b_supply = 99_0000000;
        reserve.d_supply = 65_0000000;

        let result = reserve.to_b_token_down(1_4850243);

        assert_eq!(result, 1_1234566);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1223)")]
    fn test_require_action_allowed_panics_if_supply_disabled_asset() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.enabled = false;

        reserve.require_action_allowed(&e, RequestType::SupplyCollateral as u32);
    }

    #[test]
    #[should_panic(expected = "Error(Contract, #1223)")]
    fn test_require_action_allowed_panics_if_borrow_disabled_asset() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.enabled = false;

        reserve.require_action_allowed(&e, RequestType::Borrow as u32);
    }

    #[test]
    fn test_require_action_allowed_passed_if_withdraw_or_repay() {
        let e = Env::default();

        let mut reserve = testutils::default_reserve(&e);
        reserve.enabled = false;

        reserve.require_action_allowed(&e, RequestType::Withdraw as u32);
        reserve.require_action_allowed(&e, RequestType::WithdrawCollateral as u32);
        reserve.require_action_allowed(&e, RequestType::Repay as u32);
    }

    #[test]
    fn test_gulp() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let mut reserve = testutils::default_reserve(&e);
        reserve.backstop_credit = 0_1234567;

        reserve.gulp(0_2000000, 100_0000000);
        assert_eq!(reserve.backstop_credit, 20_0000000 + 0_1234567);
        assert_eq!(reserve.b_rate, 1_800000000);
        assert_eq!(reserve.last_time, 0);
    }

    #[test]
    fn test_gulp_negative_delta_no_change() {
        let e = Env::default();
        e.mock_all_auths();

        e.ledger().set(LedgerInfo {
            timestamp: 123456 * 5,
            protocol_version: 22,
            sequence_number: 123456,
            network_id: Default::default(),
            base_reserve: 10,
            min_temp_entry_ttl: 10,
            min_persistent_entry_ttl: 10,
            max_entry_ttl: 3110400,
        });

        let mut reserve = testutils::default_reserve(&e);
        reserve.backstop_credit = 0_1234567;

        reserve.gulp(0_2000000, -10_0000000);
        assert_eq!(reserve.backstop_credit, 0_1234567);
        assert_eq!(reserve.b_rate, 1000000000);
        assert_eq!(reserve.last_time, 0);
    }
}
