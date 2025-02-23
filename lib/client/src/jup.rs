use anchor_lang::AccountDeserialize;
use anchor_lang::__private::bytemuck::Zeroable;
use anchor_lang::prelude::*;
use anchor_spl::token::Token;
use anyhow::Result;
use fixed::types::I80F48;
use openbook_v2::{
    accounts::PlaceTakeOrder,
    accounts_zerocopy,
    pubkey_option::NonZeroPubkeyOption,
    state::{BookSide, EventHeap, Market, Orderbook, Side},
};

use crate::{
    book::{amounts_from_book, Amounts},
    remaining_accounts_to_crank,
};
use jupiter_amm_interface::{
    AccountMap, Amm, KeyedAccount, Quote, QuoteParams, Side as JupiterSide, Swap,
    SwapAndAccountMetas, SwapParams,
};
/// An abstraction in order to share reserve mints and necessary data
use solana_sdk::{pubkey::Pubkey, sysvar::clock};
use std::cell::RefCell;

#[derive(Clone)]
pub struct OpenBookMarket {
    market: Market,
    event_heap: EventHeap,
    bids: BookSide,
    asks: BookSide,
    timestamp: u64,
    key: Pubkey,
    label: String,
    related_accounts: Vec<Pubkey>,
    reserve_mints: [Pubkey; 2],
    oracle_price: Option<I80F48>,
}

impl Amm for OpenBookMarket {
    fn label(&self) -> String {
        self.label.clone()
    }

    fn key(&self) -> Pubkey {
        self.key
    }

    fn program_id(&self) -> Pubkey {
        openbook_v2::id()
    }

    fn get_reserve_mints(&self) -> Vec<Pubkey> {
        self.reserve_mints.to_vec()
    }

    fn get_accounts_to_update(&self) -> Vec<Pubkey> {
        self.related_accounts.to_vec()
    }

    fn from_keyed_account(keyed_account: &KeyedAccount) -> Result<Self> {
        let market = Market::try_deserialize(&mut keyed_account.account.data.as_slice())?;
        let mut related_accounts = vec![
            market.bids,
            market.asks,
            market.event_heap,
            market.market_base_vault,
            market.market_quote_vault,
            clock::ID,
        ];

        related_accounts.extend(
            [market.oracle_a, market.oracle_b]
                .into_iter()
                .filter_map(Option::<Pubkey>::from),
        );

        Ok(OpenBookMarket {
            market,
            key: keyed_account.key,
            label: market.name().to_string(),
            related_accounts,
            reserve_mints: [market.base_mint, market.quote_mint],
            event_heap: EventHeap::zeroed(),
            bids: BookSide::zeroed(),
            asks: BookSide::zeroed(),
            oracle_price: None,
            timestamp: 0,
        })
    }

    fn update(&mut self, account_map: &AccountMap) -> Result<()> {
        let bids_data = account_map.get(&self.market.bids).unwrap();
        self.bids = BookSide::try_deserialize(&mut bids_data.data.as_slice()).unwrap();

        let asks_data = account_map.get(&self.market.asks).unwrap();
        self.asks = BookSide::try_deserialize(&mut asks_data.data.as_slice()).unwrap();

        let event_heap_data = account_map.get(&self.market.event_heap).unwrap();
        self.event_heap = EventHeap::try_deserialize(&mut event_heap_data.data.as_slice()).unwrap();

        let clock_data = account_map.get(&clock::ID).unwrap();
        let clock: Clock = bincode::deserialize(clock_data.data.as_slice())?;

        let oracle_acc =
            |nonzero_pubkey: NonZeroPubkeyOption| -> Option<accounts_zerocopy::KeyedAccount> {
                let key = Option::from(nonzero_pubkey)?;
                let account = account_map.get(&key).unwrap().clone();
                Some(accounts_zerocopy::KeyedAccount { key, account })
            };

        self.oracle_price = self.market.oracle_price(
            oracle_acc(self.market.oracle_a).as_ref(),
            oracle_acc(self.market.oracle_b).as_ref(),
            clock.slot,
        )?;

        Ok(())
    }

    fn quote(&self, quote_params: &QuoteParams) -> Result<Quote> {
        let side = if quote_params.input_mint == self.market.quote_mint {
            Side::Bid
        } else {
            Side::Ask
        };

        let input_amount = i64::try_from(quote_params.in_amount)?;

        // quote params can have exact in (which is implemented here) and exact out which is not implemented
        // check with jupiter to add to their API exact_out support
        let (max_base_lots, max_quote_lots_including_fees) = match side {
            Side::Bid => (
                self.market.max_base_lots(),
                input_amount / self.market.quote_lot_size
                    + input_amount % self.market.quote_lot_size,
            ),
            Side::Ask => (
                input_amount / self.market.base_lot_size,
                self.market.max_quote_lots(),
            ),
        };

        let bids_ref = RefCell::new(self.bids);
        let asks_ref = RefCell::new(self.asks);
        let book = Orderbook {
            bids: bids_ref.borrow_mut(),
            asks: asks_ref.borrow_mut(),
        };

        let order_amounts: Amounts = amounts_from_book(
            book,
            side,
            max_base_lots,
            max_quote_lots_including_fees,
            &self.market,
            self.oracle_price,
            self.timestamp,
        )?;

        let (in_amount, out_amount) = match side {
            Side::Bid => (
                order_amounts.total_quote_taken_native - order_amounts.fee,
                order_amounts.total_base_taken_native,
            ),
            Side::Ask => (
                order_amounts.total_base_taken_native,
                order_amounts.total_quote_taken_native + order_amounts.fee,
            ),
        };

        Ok(Quote {
            in_amount,
            out_amount,
            fee_mint: self.market.quote_mint,
            fee_amount: order_amounts.fee,
            not_enough_liquidity: order_amounts.not_enough_liquidity,
            ..Quote::default()
        })
    }

    fn get_swap_and_account_metas(&self, swap_params: &SwapParams) -> Result<SwapAndAccountMetas> {
        let SwapParams {
            in_amount,
            source_mint,
            user_destination_token_account,
            user_source_token_account,
            user_transfer_authority,
            ..
        } = swap_params;

        let source_is_quote = source_mint == &self.market.quote_mint;

        let side = if source_is_quote {
            Side::Bid
        } else {
            Side::Ask
        };

        let (user_quote_account, user_base_account) = if source_is_quote {
            (*user_source_token_account, *user_destination_token_account)
        } else {
            (*user_destination_token_account, *user_source_token_account)
        };

        let accounts = PlaceTakeOrder {
            signer: *user_transfer_authority,
            penalty_payer: *user_transfer_authority,
            market: self.key,
            market_authority: self.market.market_authority,
            bids: self.market.bids,
            asks: self.market.asks,
            user_base_account,
            user_quote_account,
            market_base_vault: self.market.market_base_vault,
            market_quote_vault: self.market.market_quote_vault,
            event_heap: self.market.event_heap,
            oracle_a: Option::from(self.market.oracle_a),
            oracle_b: Option::from(self.market.oracle_b),
            token_program: Token::id(),
            system_program: System::id(),
            open_orders_admin: None,
        };

        let mut account_metas = accounts.to_account_metas(None);

        let input_amount = i64::try_from(*in_amount)?;

        let (max_base_lots, max_quote_lots_including_fees) = match side {
            Side::Bid => (
                self.market.max_base_lots(),
                input_amount / self.market.quote_lot_size
                    + input_amount % self.market.quote_lot_size,
            ),
            Side::Ask => (
                input_amount / self.market.base_lot_size,
                self.market.max_quote_lots(),
            ),
        };

        let bids_ref = RefCell::new(self.bids);
        let asks_ref = RefCell::new(self.asks);
        let book = Orderbook {
            bids: bids_ref.borrow_mut(),
            asks: asks_ref.borrow_mut(),
        };

        let remainigs = remaining_accounts_to_crank(
            book,
            side,
            max_base_lots,
            max_quote_lots_including_fees,
            &self.market,
            self.oracle_price,
            self.timestamp,
        )?;

        let remainigs_accounts: Vec<AccountMeta> = remainigs
            .iter()
            .map(|&pubkey| AccountMeta::new(pubkey, false))
            .collect();
        account_metas.extend(remainigs_accounts);

        let side = if source_is_quote {
            JupiterSide::Bid
        } else {
            JupiterSide::Ask
        };

        Ok(SwapAndAccountMetas {
            swap: Swap::Openbook { side: { side } },
            account_metas,
        })
    }

    fn clone_amm(&self) -> Box<dyn Amm + Send + Sync> {
        Box::new(self.clone())
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use solana_client::rpc_client::RpcClient;
    use std::str::FromStr;

    #[test]
    // TODO replace std::env by mainnet market after audit deploy
    fn test_jupiter_local() -> Result<()> {
        let market = match std::env::var("MARKET_PUBKEY") {
            Ok(key) => Pubkey::from_str(&key)?,
            Err(_) => {
                println!("missing MARKET_PUBKEY env with an existing market in the local validator, skipping test");
                return Ok(());
            }
        };

        let rpc = RpcClient::new("http://127.0.0.1:8899");
        let account = rpc.get_account(&market)?;

        let market_account = KeyedAccount {
            key: market,
            account,
            params: None,
        };

        let mut openbook = OpenBookMarket::from_keyed_account(&market_account).unwrap();

        let pubkeys = openbook.get_accounts_to_update();
        let accounts: AccountMap = pubkeys
            .iter()
            .zip(rpc.get_multiple_accounts(&pubkeys)?)
            .map(|(key, acc)| (*key, acc.unwrap()))
            .collect();

        openbook.update(&accounts)?;

        let (base_mint, quote_mint) = {
            let reserves = openbook.get_reserve_mints();
            (reserves[0], reserves[1])
        };

        let quote_params = QuoteParams {
            in_amount: 80,
            input_mint: base_mint,
            output_mint: quote_mint,
        };

        let quote = openbook.quote(&quote_params)?;

        println!(
            "Market with base_lot_size = {}, quote_lot_size = {}, taker_fee = {}",
            openbook.market.base_lot_size,
            openbook.market.quote_lot_size,
            openbook.market.taker_fee
        );
        println!("{:#?}", quote_params);
        println!("{:#?}", quote);

        Ok(())
    }
}
