use super::pool::PoolState;
use crate::error::ErrorCode;
use crate::libraries::{liquidity_math, tick_math};
use crate::pool::{RewardInfo, REWARD_NUM};
use crate::util::*;
use crate::Result;
use anchor_lang::{prelude::*, system_program};
#[cfg(feature = "enable-log")]
use std::convert::identity;

pub const TICK_ARRAY_SEED: &str = "tick_array";
pub const TICK_ARRAY_SIZE_USIZE: usize = 60;
pub const TICK_ARRAY_SIZE: i32 = 60;
// pub const MIN_TICK_ARRAY_START_INDEX: i32 = -443636;
// pub const MAX_TICK_ARRAY_START_INDEX: i32 = 306600;
#[account(zero_copy(unsafe))]
#[repr(packed)]
pub struct TickArrayState {
    pub pool_id: Pubkey,
    pub start_tick_index: i32,
    pub ticks: [TickState; TICK_ARRAY_SIZE_USIZE],
    pub initialized_tick_count: u8,
    // account update recent epoch
    pub recent_epoch: u64,
    // Unused bytes for future upgrades.
    pub padding: [u8; 107],
}

impl TickArrayState {
    pub const LEN: usize = 8 + 32 + 4 + TickState::LEN * TICK_ARRAY_SIZE_USIZE + 1 + 115;

    pub fn key(&self) -> Pubkey {
        Pubkey::find_program_address(
            &[
                TICK_ARRAY_SEED.as_bytes(),
                self.pool_id.as_ref(),
                &self.start_tick_index.to_be_bytes(),
            ],
            &crate::id(),
        )
        .0
    }
    /// Load a TickArrayState of type AccountLoader from tickarray account info, if tickarray account is not exist, then create it.
    pub fn get_or_create_tick_array<'info>(
        payer: AccountInfo<'info>,
        tick_array_account_info: AccountInfo<'info>,
        system_program: AccountInfo<'info>,
        pool_state_loader: &AccountLoader<'info, PoolState>,
        tick_array_start_index: i32,
        tick_spacing: u16,
    ) -> Result<AccountLoad<'info, TickArrayState>> {
        require!(
            TickArrayState::check_is_valid_start_index(tick_array_start_index, tick_spacing),
            ErrorCode::InvaildTickIndex
        );

        let tick_array_state = if tick_array_account_info.owner == &system_program::ID {
            let (expect_pda_address, bump) = Pubkey::find_program_address(
                &[
                    TICK_ARRAY_SEED.as_bytes(),
                    pool_state_loader.key().as_ref(),
                    &tick_array_start_index.to_be_bytes(),
                ],
                &crate::id(),
            );
            require_keys_eq!(expect_pda_address, tick_array_account_info.key());
            create_or_allocate_account(
                &crate::id(),
                payer,
                system_program,
                tick_array_account_info.clone(),
                &[
                    TICK_ARRAY_SEED.as_bytes(),
                    pool_state_loader.key().as_ref(),
                    &tick_array_start_index.to_be_bytes(),
                    &[bump],
                ],
                TickArrayState::LEN,
            )?;
            let tick_array_state_loader = AccountLoad::<TickArrayState>::try_from_unchecked(
                &crate::id(),
                &tick_array_account_info,
            )?;
            {
                let mut tick_array_account = tick_array_state_loader.load_init()?;
                tick_array_account.initialize(
                    tick_array_start_index,
                    tick_spacing,
                    pool_state_loader.key(),
                )?;
            }
            tick_array_state_loader
        } else {
            AccountLoad::<TickArrayState>::try_from(&tick_array_account_info)?
        };
        Ok(tick_array_state)
    }

    /**
     * Initialize only can be called when first created
     */
    pub fn initialize(
        &mut self,
        start_index: i32,
        tick_spacing: u16,
        pool_key: Pubkey,
    ) -> Result<()> {
        TickArrayState::check_is_valid_start_index(start_index, tick_spacing);
        self.start_tick_index = start_index;
        self.pool_id = pool_key;
        self.recent_epoch = Clock::get()?.epoch;
        Ok(())
    }

    pub fn update_initialized_tick_count(&mut self, add: bool) -> Result<()> {
        if add {
            self.initialized_tick_count += 1;
        } else {
            self.initialized_tick_count -= 1;
        }
        Ok(())
    }

    pub fn get_tick_state_mut(
        &mut self,
        tick_index: i32,
        tick_spacing: u16,
    ) -> Result<&mut TickState> {
        let offset_in_array = self.get_tick_offset_in_array(tick_index, tick_spacing)?;
        Ok(&mut self.ticks[offset_in_array])
    }

    pub fn update_tick_state(
        &mut self,
        tick_index: i32,
        tick_spacing: u16,
        tick_state: TickState,
    ) -> Result<()> {
        let offset_in_array = self.get_tick_offset_in_array(tick_index, tick_spacing)?;
        self.ticks[offset_in_array] = tick_state;
        self.recent_epoch = Clock::get()?.epoch;
        Ok(())
    }

    /// Get tick's offset in current tick array, tick must be include in tick array， otherwise throw an error
    fn get_tick_offset_in_array(self, tick_index: i32, tick_spacing: u16) -> Result<usize> {
        let start_tick_index = TickArrayState::get_array_start_index(tick_index, tick_spacing);
        require_eq!(
            start_tick_index,
            self.start_tick_index,
            ErrorCode::InvalidTickArray
        );
        let offset_in_array =
            ((tick_index - self.start_tick_index) / i32::from(tick_spacing)) as usize;
        Ok(offset_in_array)
    }

    /// Base on swap directioin, return the first initialized tick in the tick array.
    pub fn first_initialized_tick(&self, zero_for_one: bool) -> Result<&TickState> {
        if zero_for_one {
            let mut i = TICK_ARRAY_SIZE - 1;
            while i >= 0 {
                if self.ticks[i as usize].is_initialized() {
                    return Ok(self.ticks.get(i as usize).unwrap());
                }
                i = i - 1;
            }
        } else {
            let mut i = 0;
            while i < TICK_ARRAY_SIZE_USIZE {
                if self.ticks[i].is_initialized() {
                    return Ok(self.ticks.get(i).unwrap());
                }
                i = i + 1;
            }
        }
        err!(ErrorCode::InvalidTickArray)
    }

    /// Get next initialized tick in tick array, `current_tick_index` can be any tick index, in other words, `current_tick_index` not exactly a point in the tickarray,
    /// and current_tick_index % tick_spacing maybe not equal zero.
    /// If price move to left tick <= current_tick_index, or to right tick > current_tick_index
    pub fn next_initialized_tick(
        &self,
        current_tick_index: i32,
        tick_spacing: u16,
        zero_for_one: bool,
    ) -> Result<Option<&TickState>> {
        let current_tick_array_start_index =
            TickArrayState::get_array_start_index(current_tick_index, tick_spacing);
        if current_tick_array_start_index != self.start_tick_index {
            return Ok(None);
        }
        let mut offset_in_array =
            (current_tick_index - self.start_tick_index) / i32::from(tick_spacing);

        if zero_for_one {
            while offset_in_array >= 0 {
                if self.ticks[offset_in_array as usize].is_initialized() {
                    return Ok(self.ticks.get(offset_in_array as usize));
                }
                offset_in_array = offset_in_array - 1;
            }
        } else {
            offset_in_array = offset_in_array + 1;
            while offset_in_array < TICK_ARRAY_SIZE {
                if self.ticks[offset_in_array as usize].is_initialized() {
                    return Ok(self.ticks.get(offset_in_array as usize));
                }
                offset_in_array = offset_in_array + 1;
            }
        }
        Ok(None)
    }

    /// Base on swap directioin, return the next tick array start index.
    pub fn next_tick_arrary_start_index(&self, tick_spacing: u16, zero_for_one: bool) -> i32 {
        let ticks_in_array = TICK_ARRAY_SIZE * i32::from(tick_spacing);
        if zero_for_one {
            self.start_tick_index - ticks_in_array
        } else {
            self.start_tick_index + ticks_in_array
        }
    }

    /// Input an arbitrary tick_index, output the start_index of the tick_array it sits on
    pub fn get_array_start_index(tick_index: i32, tick_spacing: u16) -> i32 {
        let ticks_in_array = TickArrayState::tick_count(tick_spacing);
        let mut start = tick_index / ticks_in_array;
        if tick_index < 0 && tick_index % ticks_in_array != 0 {
            start = start - 1
        }
        start * ticks_in_array
    }

    pub fn check_is_valid_start_index(tick_index: i32, tick_spacing: u16) -> bool {
        if TickState::check_is_out_of_boundary(tick_index) {
            if tick_index > tick_math::MAX_TICK {
                return false;
            }
            let min_start_index =
                TickArrayState::get_array_start_index(tick_math::MIN_TICK, tick_spacing);
            return tick_index == min_start_index;
        }
        tick_index % TickArrayState::tick_count(tick_spacing) == 0
    }

    pub fn tick_count(tick_spacing: u16) -> i32 {
        TICK_ARRAY_SIZE * i32::from(tick_spacing)
    }
}

impl Default for TickArrayState {
    #[inline]
    fn default() -> TickArrayState {
        TickArrayState {
            pool_id: Pubkey::default(),
            ticks: [TickState::default(); TICK_ARRAY_SIZE_USIZE],
            start_tick_index: 0,
            initialized_tick_count: 0,
            recent_epoch: 0,
            padding: [0; 107],
        }
    }
}

#[zero_copy(unsafe)]
#[repr(packed)]
#[derive(Default, Debug)]
pub struct TickState {
    pub tick: i32,
    /// Amount of net liquidity added (subtracted) when tick is crossed from left to right (right to left)
    pub liquidity_net: i128,
    /// The total position liquidity that references this tick
    pub liquidity_gross: u128,

    /// Fee growth per unit of liquidity on the _other_ side of this tick (relative to the current tick)
    /// only has relative meaning, not absolute — the value depends on when the tick is initialized
    pub fee_growth_outside_0_x64: u128,
    pub fee_growth_outside_1_x64: u128,

    // Reward growth per unit of liquidity like fee, array of Q64.64
    pub reward_growths_outside_x64: [u128; REWARD_NUM],
    // Unused bytes for future upgrades.
    pub padding: [u32; 13],
}

impl TickState {
    pub const LEN: usize = 4 + 16 + 16 + 16 + 16 + 16 * REWARD_NUM + 16 + 16 + 8 + 8 + 4;

    pub fn initialize(&mut self, tick: i32, tick_spacing: u16) -> Result<()> {
        if TickState::check_is_out_of_boundary(tick) {
            return err!(ErrorCode::InvaildTickIndex);
        }
        require!(
            tick % i32::from(tick_spacing) == 0,
            ErrorCode::TickAndSpacingNotMatch
        );
        self.tick = tick;
        Ok(())
    }
    /// Updates a tick and returns true if the tick was flipped from initialized to uninitialized
    pub fn update(
        &mut self,
        tick_current: i32,
        liquidity_delta: i128,
        fee_growth_global_0_x64: u128,
        fee_growth_global_1_x64: u128,
        upper: bool,
        reward_infos: &[RewardInfo; REWARD_NUM],
    ) -> Result<bool> {
        let liquidity_gross_before = self.liquidity_gross;
        let liquidity_gross_after =
            liquidity_math::add_delta(liquidity_gross_before, liquidity_delta)?;

        // Either liquidity_gross_after becomes 0 (uninitialized) XOR liquidity_gross_before
        // was zero (initialized)
        let flipped = (liquidity_gross_after == 0) != (liquidity_gross_before == 0);
        if liquidity_gross_before == 0 {
            // by convention, we assume that all growth before a tick was initialized happened _below_ the tick
            if self.tick <= tick_current {
                self.fee_growth_outside_0_x64 = fee_growth_global_0_x64;
                self.fee_growth_outside_1_x64 = fee_growth_global_1_x64;
                self.reward_growths_outside_x64 = RewardInfo::get_reward_growths(reward_infos);
            }
        }

        self.liquidity_gross = liquidity_gross_after;

        // when the lower (upper) tick is crossed left to right (right to left),
        // liquidity must be added (removed)
        self.liquidity_net = if upper {
            self.liquidity_net.checked_sub(liquidity_delta)
        } else {
            self.liquidity_net.checked_add(liquidity_delta)
        }
        .unwrap();
        Ok(flipped)
    }

    /// Transitions to the current tick as needed by price movement, returning the amount of liquidity
    /// added (subtracted) when tick is crossed from left to right (right to left)
    pub fn cross(
        &mut self,
        fee_growth_global_0_x64: u128,
        fee_growth_global_1_x64: u128,
        reward_infos: &[RewardInfo; REWARD_NUM],
    ) -> i128 {
        self.fee_growth_outside_0_x64 = fee_growth_global_0_x64
            .checked_sub(self.fee_growth_outside_0_x64)
            .unwrap();
        self.fee_growth_outside_1_x64 = fee_growth_global_1_x64
            .checked_sub(self.fee_growth_outside_1_x64)
            .unwrap();

        for i in 0..REWARD_NUM {
            if !reward_infos[i].initialized() {
                continue;
            }

            self.reward_growths_outside_x64[i] = reward_infos[i]
                .reward_growth_global_x64
                .checked_sub(self.reward_growths_outside_x64[i])
                .unwrap();
        }

        self.liquidity_net
    }

    pub fn clear(&mut self) {
        self.liquidity_net = 0;
        self.liquidity_gross = 0;
        self.fee_growth_outside_0_x64 = 0;
        self.fee_growth_outside_1_x64 = 0;
        self.reward_growths_outside_x64 = [0; REWARD_NUM];
    }

    pub fn is_initialized(self) -> bool {
        self.liquidity_gross != 0
    }

    /// Common checks for a valid tick input.
    /// A tick is valid if it lies within tick boundaries
    pub fn check_is_out_of_boundary(tick: i32) -> bool {
        tick < tick_math::MIN_TICK || tick > tick_math::MAX_TICK
    }
}

// Calculates the fee growths inside of tick_lower and tick_upper based on their positions relative to tick_current.
/// `fee_growth_inside = fee_growth_global - fee_growth_below(lower) - fee_growth_above(upper)`
///
pub fn get_fee_growth_inside(
    tick_lower: &TickState,
    tick_upper: &TickState,
    tick_current: i32,
    fee_growth_global_0_x64: u128,
    fee_growth_global_1_x64: u128,
) -> (u128, u128) {
    // calculate fee growth below
    let (fee_growth_below_0_x64, fee_growth_below_1_x64) = if tick_current >= tick_lower.tick {
        (
            tick_lower.fee_growth_outside_0_x64,
            tick_lower.fee_growth_outside_1_x64,
        )
    } else {
        (
            fee_growth_global_0_x64
                .checked_sub(tick_lower.fee_growth_outside_0_x64)
                .unwrap(),
            fee_growth_global_1_x64
                .checked_sub(tick_lower.fee_growth_outside_1_x64)
                .unwrap(),
        )
    };

    // Calculate fee growth above
    let (fee_growth_above_0_x64, fee_growth_above_1_x64) = if tick_current < tick_upper.tick {
        (
            tick_upper.fee_growth_outside_0_x64,
            tick_upper.fee_growth_outside_1_x64,
        )
    } else {
        (
            fee_growth_global_0_x64
                .checked_sub(tick_upper.fee_growth_outside_0_x64)
                .unwrap(),
            fee_growth_global_1_x64
                .checked_sub(tick_upper.fee_growth_outside_1_x64)
                .unwrap(),
        )
    };
    let fee_growth_inside_0_x64 = fee_growth_global_0_x64
        .wrapping_sub(fee_growth_below_0_x64)
        .wrapping_sub(fee_growth_above_0_x64);
    let fee_growth_inside_1_x64 = fee_growth_global_1_x64
        .wrapping_sub(fee_growth_below_1_x64)
        .wrapping_sub(fee_growth_above_1_x64);

    (fee_growth_inside_0_x64, fee_growth_inside_1_x64)
}

// Calculates the reward growths inside of tick_lower and tick_upper based on their positions relative to tick_current.
pub fn get_reward_growths_inside(
    tick_lower: &TickState,
    tick_upper: &TickState,
    tick_current_index: i32,
    reward_infos: &[RewardInfo; REWARD_NUM],
) -> [u128; REWARD_NUM] {
    let mut reward_growths_inside = [0; REWARD_NUM];

    for i in 0..REWARD_NUM {
        if !reward_infos[i].initialized() {
            continue;
        }

        let reward_growths_below = if tick_current_index >= tick_lower.tick {
            tick_lower.reward_growths_outside_x64[i]
        } else {
            reward_infos[i]
                .reward_growth_global_x64
                .checked_sub(tick_lower.reward_growths_outside_x64[i])
                .unwrap()
        };

        let reward_growths_above = if tick_current_index < tick_upper.tick {
            tick_upper.reward_growths_outside_x64[i]
        } else {
            reward_infos[i]
                .reward_growth_global_x64
                .checked_sub(tick_upper.reward_growths_outside_x64[i])
                .unwrap()
        };
        reward_growths_inside[i] = reward_infos[i]
            .reward_growth_global_x64
            .wrapping_sub(reward_growths_below)
            .wrapping_sub(reward_growths_above);
        #[cfg(feature = "enable-log")]
        msg!(
            "get_reward_growths_inside,i:{},reward_growth_global:{},reward_growth_below:{},reward_growth_above:{}, reward_growth_inside:{}",
            i,
            identity(reward_infos[i].reward_growth_global_x64),
            reward_growths_below,
            reward_growths_above,
            reward_growths_inside[i]
        );
    }

    reward_growths_inside
}

pub fn check_tick_array_start_index(
    tick_array_start_index: i32,
    tick_index: i32,
    tick_spacing: u16,
) -> Result<()> {
    require!(
        tick_index >= tick_math::MIN_TICK,
        ErrorCode::TickLowerOverflow
    );
    require!(
        tick_index <= tick_math::MAX_TICK,
        ErrorCode::TickUpperOverflow
    );
    require_eq!(0, tick_index % i32::from(tick_spacing));
    let expect_start_index = TickArrayState::get_array_start_index(tick_index, tick_spacing);
    require_eq!(tick_array_start_index, expect_start_index);
    Ok(())
}

/// Common checks for valid tick inputs.
///
pub fn check_ticks_order(tick_lower_index: i32, tick_upper_index: i32) -> Result<()> {
    require!(
        tick_lower_index < tick_upper_index,
        ErrorCode::TickInvaildOrder
    );
    Ok(())
}

#[cfg(test)]
pub mod tick_array_test {
    use super::*;
    use std::cell::RefCell;

    pub struct TickArrayInfo {
        pub start_tick_index: i32,
        pub ticks: Vec<TickState>,
    }

    pub fn build_tick_array(
        start_index: i32,
        tick_spacing: u16,
        initialized_tick_offsets: Vec<usize>,
    ) -> RefCell<TickArrayState> {
        let mut new_tick_array = TickArrayState::default();
        new_tick_array
            .initialize(start_index, tick_spacing, Pubkey::default())
            .unwrap();

        for offset in initialized_tick_offsets {
            let mut new_tick = TickState::default();
            // Indicates tick is initialized
            new_tick.liquidity_gross = 1;
            new_tick.tick = start_index + (offset * tick_spacing as usize) as i32;
            new_tick_array.ticks[offset] = new_tick;
        }
        RefCell::new(new_tick_array)
    }

    pub fn build_tick_array_with_tick_states(
        pool_id: Pubkey,
        start_index: i32,
        tick_spacing: u16,
        tick_states: Vec<TickState>,
    ) -> RefCell<TickArrayState> {
        let mut new_tick_array = TickArrayState::default();
        new_tick_array
            .initialize(start_index, tick_spacing, pool_id)
            .unwrap();

        for tick_state in tick_states {
            assert!(tick_state.tick != 0);
            let offset = new_tick_array
                .get_tick_offset_in_array(tick_state.tick, tick_spacing)
                .unwrap();
            new_tick_array.ticks[offset] = tick_state;
        }
        RefCell::new(new_tick_array)
    }

    pub fn build_tick(tick: i32, liquidity_gross: u128, liquidity_net: i128) -> RefCell<TickState> {
        let mut new_tick = TickState::default();
        new_tick.tick = tick;
        new_tick.liquidity_gross = liquidity_gross;
        new_tick.liquidity_net = liquidity_net;
        RefCell::new(new_tick)
    }

    fn build_tick_with_fee_reward_growth(
        tick: i32,
        fee_growth_outside_0_x64: u128,
        fee_growth_outside_1_x64: u128,
        reward_growths_outside_x64: u128,
    ) -> RefCell<TickState> {
        let mut new_tick = TickState::default();
        new_tick.tick = tick;
        new_tick.fee_growth_outside_0_x64 = fee_growth_outside_0_x64;
        new_tick.fee_growth_outside_1_x64 = fee_growth_outside_1_x64;
        new_tick.reward_growths_outside_x64 = [reward_growths_outside_x64, 0, 0];
        RefCell::new(new_tick)
    }

    mod tick_array_test {
        use super::*;
        use std::convert::identity;

        #[test]
        fn get_array_start_index_test() {
            assert_eq!(TickArrayState::get_array_start_index(120, 3), 0);
            assert_eq!(TickArrayState::get_array_start_index(1002, 30), 0);
            assert_eq!(TickArrayState::get_array_start_index(-120, 3), -180);
            assert_eq!(TickArrayState::get_array_start_index(-1002, 30), -1800);
            assert_eq!(TickArrayState::get_array_start_index(-20, 10), -600);
            assert_eq!(TickArrayState::get_array_start_index(20, 10), 0);
            assert_eq!(TickArrayState::get_array_start_index(-1002, 10), -1200);
            assert_eq!(TickArrayState::get_array_start_index(-600, 10), -600);
            assert_eq!(TickArrayState::get_array_start_index(-30720, 1), -30720);
            assert_eq!(TickArrayState::get_array_start_index(30720, 1), 30720);
            assert_eq!(
                TickArrayState::get_array_start_index(tick_math::MIN_TICK, 1),
                -443640
            );
            assert_eq!(
                TickArrayState::get_array_start_index(tick_math::MAX_TICK, 1),
                443580
            );
            assert_eq!(
                TickArrayState::get_array_start_index(tick_math::MAX_TICK, 60),
                442800
            );
            assert_eq!(
                TickArrayState::get_array_start_index(tick_math::MIN_TICK, 60),
                -446400
            );
        }

        #[test]
        fn next_tick_arrary_start_index_test() {
            let tick_spacing = 15;
            let tick_array_ref = build_tick_array(-1800, tick_spacing, vec![]);
            // zero_for_one, next tickarray start_index < current
            assert_eq!(
                -2700,
                tick_array_ref
                    .borrow()
                    .next_tick_arrary_start_index(tick_spacing, true)
            );
            // one_for_zero, next tickarray start_index > current
            assert_eq!(
                -900,
                tick_array_ref
                    .borrow()
                    .next_tick_arrary_start_index(tick_spacing, false)
            );
        }

        #[test]
        fn get_tick_offset_in_array_test() {
            let tick_spacing = 4;
            // tick range [960, 1196]
            let tick_array_ref = build_tick_array(960, tick_spacing, vec![]);

            // not in tickarray
            assert_eq!(
                tick_array_ref
                    .borrow()
                    .get_tick_offset_in_array(808, tick_spacing)
                    .unwrap_err(),
                error!(ErrorCode::InvalidTickArray)
            );
            // first index is tickarray start tick
            assert_eq!(
                tick_array_ref
                    .borrow()
                    .get_tick_offset_in_array(960, tick_spacing)
                    .unwrap(),
                0
            );
            // tick_index % tick_spacing != 0
            assert_eq!(
                tick_array_ref
                    .borrow()
                    .get_tick_offset_in_array(1105, tick_spacing)
                    .unwrap(),
                36
            );
            // (1108-960) / tick_spacing
            assert_eq!(
                tick_array_ref
                    .borrow()
                    .get_tick_offset_in_array(1108, tick_spacing)
                    .unwrap(),
                37
            );
            // the end index of tickarray
            assert_eq!(
                tick_array_ref
                    .borrow()
                    .get_tick_offset_in_array(1196, tick_spacing)
                    .unwrap(),
                59
            );
        }

        #[test]
        fn first_initialized_tick_test() {
            let tick_spacing = 15;
            // initialized ticks[-300,-15]
            let tick_array_ref = build_tick_array(-900, tick_spacing, vec![40, 59]);
            let mut tick_array = tick_array_ref.borrow_mut();
            // one_for_zero, the price increase, tick from small to large
            let tick = tick_array.first_initialized_tick(false).unwrap().tick;
            assert_eq!(-300, tick);
            // zero_for_one, the price decrease, tick from large to small
            let tick = tick_array.first_initialized_tick(true).unwrap().tick;
            assert_eq!(-15, tick);
        }

        #[test]
        fn next_initialized_tick_when_tick_is_positive() {
            // init tick_index [0,30,105]
            let tick_array_ref = build_tick_array(0, 15, vec![0, 2, 7]);
            let mut tick_array = tick_array_ref.borrow_mut();

            // test zero_for_one
            let mut next_tick_state = tick_array.next_initialized_tick(0, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 0);

            next_tick_state = tick_array.next_initialized_tick(1, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 0);

            next_tick_state = tick_array.next_initialized_tick(29, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 0);
            next_tick_state = tick_array.next_initialized_tick(30, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 30);
            next_tick_state = tick_array.next_initialized_tick(31, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 30);

            // test one for zero
            let mut next_tick_state = tick_array.next_initialized_tick(0, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 30);

            next_tick_state = tick_array.next_initialized_tick(29, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 30);
            next_tick_state = tick_array.next_initialized_tick(30, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 105);
            next_tick_state = tick_array.next_initialized_tick(31, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), 105);

            next_tick_state = tick_array.next_initialized_tick(105, 15, false).unwrap();
            assert!(next_tick_state.is_none());

            // tick not in tickarray
            next_tick_state = tick_array.next_initialized_tick(900, 15, false).unwrap();
            assert!(next_tick_state.is_none());
        }

        #[test]
        fn next_initialized_tick_when_tick_is_negative() {
            // init tick_index [-900,-870,-795]
            let tick_array_ref = build_tick_array(-900, 15, vec![0, 2, 7]);
            let mut tick_array = tick_array_ref.borrow_mut();

            // test zero for one
            let mut next_tick_state = tick_array.next_initialized_tick(-900, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -900);

            next_tick_state = tick_array.next_initialized_tick(-899, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -900);

            next_tick_state = tick_array.next_initialized_tick(-871, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -900);
            next_tick_state = tick_array.next_initialized_tick(-870, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -870);
            next_tick_state = tick_array.next_initialized_tick(-869, 15, true).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -870);

            // test one for zero
            let mut next_tick_state = tick_array.next_initialized_tick(-900, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -870);

            next_tick_state = tick_array.next_initialized_tick(-871, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -870);
            next_tick_state = tick_array.next_initialized_tick(-870, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -795);
            next_tick_state = tick_array.next_initialized_tick(-869, 15, false).unwrap();
            assert_eq!(identity(next_tick_state.unwrap().tick), -795);

            next_tick_state = tick_array.next_initialized_tick(-795, 15, false).unwrap();
            assert!(next_tick_state.is_none());

            // tick not in tickarray
            next_tick_state = tick_array.next_initialized_tick(-10, 15, false).unwrap();
            assert!(next_tick_state.is_none());
        }
    }

    mod get_fee_growth_inside_test {
        use super::*;
        use crate::states::{
            pool::RewardInfo,
            tick_array::{get_fee_growth_inside, TickState},
        };

        fn fee_growth_inside_delta_when_price_move(
            init_fee_growth_global_0_x64: u128,
            init_fee_growth_global_1_x64: u128,
            fee_growth_global_delta: u128,
            mut tick_current: i32,
            target_tick_current: i32,
            tick_lower: &mut TickState,
            tick_upper: &mut TickState,
            cross_tick_lower: bool,
        ) -> (u128, u128) {
            let mut fee_growth_global_0_x64 = init_fee_growth_global_0_x64;
            let mut fee_growth_global_1_x64 = init_fee_growth_global_1_x64;
            let (fee_growth_inside_0_before, fee_growth_inside_1_before) = get_fee_growth_inside(
                tick_lower,
                tick_upper,
                tick_current,
                fee_growth_global_0_x64,
                fee_growth_global_1_x64,
            );

            if fee_growth_global_0_x64 != 0 {
                fee_growth_global_0_x64 = fee_growth_global_0_x64 + fee_growth_global_delta;
            }
            if fee_growth_global_1_x64 != 0 {
                fee_growth_global_1_x64 = fee_growth_global_1_x64 + fee_growth_global_delta;
            }
            if cross_tick_lower {
                tick_lower.cross(
                    fee_growth_global_0_x64,
                    fee_growth_global_1_x64,
                    &[RewardInfo::default(); 3],
                );
            } else {
                tick_upper.cross(
                    fee_growth_global_0_x64,
                    fee_growth_global_1_x64,
                    &[RewardInfo::default(); 3],
                );
            }

            tick_current = target_tick_current;
            let (fee_growth_inside_0_after, fee_growth_inside_1_after) = get_fee_growth_inside(
                tick_lower,
                tick_upper,
                tick_current,
                fee_growth_global_0_x64,
                fee_growth_global_1_x64,
            );

            println!(
                "inside_delta_0:{},fee_growth_inside_0_after:{},fee_growth_inside_0_before:{}",
                fee_growth_inside_0_after.wrapping_sub(fee_growth_inside_0_before),
                fee_growth_inside_0_after,
                fee_growth_inside_0_before
            );
            println!(
                "inside_delta_1:{},fee_growth_inside_1_after:{},fee_growth_inside_1_before:{}",
                fee_growth_inside_1_after.wrapping_sub(fee_growth_inside_1_before),
                fee_growth_inside_1_after,
                fee_growth_inside_1_before
            );
            (
                fee_growth_inside_0_after.wrapping_sub(fee_growth_inside_0_before),
                fee_growth_inside_1_after.wrapping_sub(fee_growth_inside_1_before),
            )
        }

        #[test]
        fn price_in_tick_range_move_to_right_test() {
            // one_for_zero, price move to right and token_1 fee growth

            // tick_lower and tick_upper all new create, and tick_lower initialize with fee_growth_global_1_x64(1000)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    0,
                    11,
                    build_tick_with_fee_reward_growth(-10, 0, 1000, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 500);

            // tick_lower is initialized with fee_growth_outside_1_x64(100) and tick_upper is new create.
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    0,
                    11,
                    build_tick_with_fee_reward_growth(-10, 0, 100, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 500);

            // tick_lower is new create with fee_growth_global_1_x64(1000)  and tick_upper is initialized with fee_growth_outside_1_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    0,
                    11,
                    build_tick_with_fee_reward_growth(-10, 0, 1000, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 100, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 500);

            // tick_lower is initialized with fee_growth_outside_1_x64(50)  and tick_upper is initialized with fee_growth_outside_1_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    0,
                    11,
                    build_tick_with_fee_reward_growth(-10, 0, 50, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 100, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 500);
        }

        #[test]
        fn price_in_tick_range_move_to_left_test() {
            // zero_for_one, price move to left and token_0 fee growth

            // tick_lower and tick_upper all new create, and tick_lower initialize with fee_growth_global_0_x64(1000)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    0,
                    -11,
                    build_tick_with_fee_reward_growth(-10, 1000, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 500);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is initialized with fee_growth_outside_0_x64(100) and tick_upper is new create.
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    0,
                    -11,
                    build_tick_with_fee_reward_growth(-10, 100, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 500);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is new create with fee_growth_global_0_x64(1000)  and tick_upper is initialized with fee_growth_outside_0_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    0,
                    -11,
                    build_tick_with_fee_reward_growth(-10, 1000, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 100, 0, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 500);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is initialized with fee_growth_outside_0_x64(50)  and tick_upper is initialized with fee_growth_outside_0_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    0,
                    -11,
                    build_tick_with_fee_reward_growth(-10, 50, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 100, 0, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 500);
            assert_eq!(fee_growth_inside_delta_1, 0);
        }

        #[test]
        fn price_in_tick_range_left_move_to_right_test() {
            // one_for_zero, price move to right and token_1 fee growth

            // tick_lower and tick_upper all new create
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    -11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 0, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is initialized with fee_growth_outside_1_x64(100) and tick_upper is new create.
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    -11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 0, 100, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is new create  and tick_upper is initialized with fee_growth_outside_1_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    -11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 0, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 100, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is initialized with fee_growth_outside_1_x64(50)  and tick_upper is initialized with fee_growth_outside_1_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    0,
                    1000,
                    500,
                    -11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 0, 50, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 0, 100, 0).get_mut(),
                    true,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);
        }

        #[test]
        fn price_in_tick_range_right_move_to_left_test() {
            // zero_for_one, price move to left and token_0 fee growth

            // tick_lower and tick_upper all new create
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 1000, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 1000, 0, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is initialized with fee_growth_outside_0_x64(100) and tick_upper is new create.
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 100, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 1000, 0, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is new create with fee_growth_global_0_x64(1000)  and tick_upper is initialized with fee_growth_outside_0_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 1000, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 100, 0, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);

            // tick_lower is initialized with fee_growth_outside_0_x64(50)  and tick_upper is initialized with fee_growth_outside_0_x64(100)
            let (fee_growth_inside_delta_0, fee_growth_inside_delta_1) =
                fee_growth_inside_delta_when_price_move(
                    1000,
                    0,
                    500,
                    11,
                    0,
                    build_tick_with_fee_reward_growth(-10, 50, 0, 0).get_mut(),
                    build_tick_with_fee_reward_growth(10, 100, 0, 0).get_mut(),
                    false,
                );
            assert_eq!(fee_growth_inside_delta_0, 0);
            assert_eq!(fee_growth_inside_delta_1, 0);
        }
    }

    mod get_reward_growths_inside_test {
        use super::*;
        use crate::states::{
            pool::RewardInfo,
            tick_array::{get_reward_growths_inside, TickState},
        };
        use anchor_lang::prelude::Pubkey;

        fn build_reward_infos(reward_growth_global_x64: u128) -> [RewardInfo; 3] {
            [
                RewardInfo {
                    token_mint: Pubkey::new_unique(),
                    reward_growth_global_x64,
                    ..Default::default()
                },
                RewardInfo::default(),
                RewardInfo::default(),
            ]
        }

        fn reward_growth_inside_delta_when_price_move(
            init_reward_growth_global_x64: u128,
            reward_growth_global_delta: u128,
            mut tick_current: i32,
            target_tick_current: i32,
            tick_lower: &mut TickState,
            tick_upper: &mut TickState,
            cross_tick_lower: bool,
        ) -> u128 {
            let mut reward_growth_global_x64 = init_reward_growth_global_x64;
            let reward_growth_inside_before = get_reward_growths_inside(
                tick_lower,
                tick_upper,
                tick_current,
                &build_reward_infos(reward_growth_global_x64),
            )[0];

            reward_growth_global_x64 = reward_growth_global_x64 + reward_growth_global_delta;
            if cross_tick_lower {
                tick_lower.cross(0, 0, &build_reward_infos(reward_growth_global_x64));
            } else {
                tick_upper.cross(0, 0, &build_reward_infos(reward_growth_global_x64));
            }

            tick_current = target_tick_current;
            let reward_growth_inside_after = get_reward_growths_inside(
                tick_lower,
                tick_upper,
                tick_current,
                &build_reward_infos(reward_growth_global_x64),
            )[0];

            println!(
                "inside_delta:{}, reward_growth_inside_after:{}, reward_growth_inside_before:{}",
                reward_growth_inside_after.wrapping_sub(reward_growth_inside_before),
                reward_growth_inside_after,
                reward_growth_inside_before,
            );

            reward_growth_inside_after.wrapping_sub(reward_growth_inside_before)
        }

        #[test]
        fn uninitialized_reward_index_test() {
            let tick_current = 0;

            let tick_lower = &mut TickState {
                tick: -10,
                reward_growths_outside_x64: [1000, 0, 0],
                ..Default::default()
            };
            let tick_upper = &mut TickState {
                tick: 10,
                reward_growths_outside_x64: [1000, 0, 0],
                ..Default::default()
            };

            let reward_infos = &[RewardInfo::default(); 3];
            let reward_inside =
                get_reward_growths_inside(tick_lower, tick_upper, tick_current, reward_infos);
            assert_eq!(reward_inside, [0; 3]);
        }

        #[test]
        fn price_in_tick_range_move_to_right_test() {
            // tick_lower and tick_upper all new create
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 1000).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                false,
            );
            assert_eq!(reward_frowth_inside_delta, 500);

            // tick_lower is initialized with reward_growths_outside_x64(100) and tick_upper is new create.
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 100).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                false,
            );
            assert_eq!(reward_frowth_inside_delta, 500);

            // tick_lower is new create with reward_growths_outside_x64(1000)  and tick_upper is initialized with reward_growths_outside_x64(100)
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 1000).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 100).get_mut(),
                false,
            );
            assert_eq!(reward_frowth_inside_delta, 500);

            // tick_lower is initialized with reward_growths_outside_x64(50)  and tick_upper is initialized with reward_growths_outside_x64(100)
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 50).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 100).get_mut(),
                false,
            );
            assert_eq!(reward_frowth_inside_delta, 500);
        }

        #[test]
        fn price_in_tick_range_move_to_left_test() {
            // zero_for_one, cross tick_lower

            // tick_lower and tick_upper all new create, and tick_lower initialize with reward_growths_outside_x64(1000)
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                -11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 1000).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                true,
            );
            assert_eq!(reward_frowth_inside_delta, 500);

            // tick_lower is initialized with reward_growths_outside_x64(100) and tick_upper is new create.
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                -11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 100).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 0).get_mut(),
                true,
            );
            assert_eq!(reward_frowth_inside_delta, 500);

            // tick_lower is new create with reward_growths_outside_x64(1000)  and tick_upper is initialized with reward_growths_outside_x64(100)
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                -11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 1000).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 100).get_mut(),
                true,
            );
            assert_eq!(reward_frowth_inside_delta, 500);

            // tick_lower is initialized with reward_growths_outside_x64(50)  and tick_upper is initialized with reward_growths_outside_x64(100)
            let reward_frowth_inside_delta = reward_growth_inside_delta_when_price_move(
                1000,
                500,
                0,
                -11,
                build_tick_with_fee_reward_growth(-10, 0, 0, 50).get_mut(),
                build_tick_with_fee_reward_growth(10, 0, 0, 100).get_mut(),
                true,
            );
            assert_eq!(reward_frowth_inside_delta, 500);
        }
    }
}
