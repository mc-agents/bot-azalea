use azalea::Client;
use azalea::inventory::components::{Enchantments, StoredEnchantments};
use azalea::inventory::item::MaxStackSizeExt;
use azalea::inventory::{ItemStack, Menu};
use azalea::protocol::packets::game::c_merchant_offers::{ClientboundMerchantOffers, ItemCost, MerchantOffer};
use azalea::protocol::packets::game::s_select_trade::ServerboundSelectTrade;
use azalea::registry::DataRegistry;
use serde_json::{Value, json};

use super::args::text;
use super::text::component;
use super::windows::{Window, menu, resync, screen};
use super::{Tool, alive, in_world, stacks};
use crate::calls::{Answer, Failure};
use crate::menus::{Known, Menus};

/// The merchant menu's own layout: two payment slots, then the result, then the inventory.
const PAYMENT_A: usize = 0;
const PAYMENT_B: usize = 1;
const RESULT: usize = 2;

/// The trading screen and the offers the server sent for it. The offers come in a packet of their
/// own a moment after the window, so a screen read in that moment is open with none.
fn require(client: &Client) -> Result<(Window, Option<ClientboundMerchantOffers>), Failure> {
    let window = menu(client).filter(|window| matches!(window.menu, Menu::Merchant { .. }));
    let Some(window) = window else {
        let book = client
            .get_component::<Menus>()
            .is_some_and(|menus| menus.book.is_some());
        let showing = menu(client)
            .map(|window| screen(&window.menu))
            .or(book.then_some("BookViewScreen"));
        return Err(Failure::refused(
            "NO_TRADES",
            match showing {
                None => "no trading screen is open. Use interact-entity on a villager or a wandering trader, then wait-for-window.".to_owned(),
                Some(screen) => format!("{screen} is open, and it is not a trading screen."),
            },
        ));
    };
    let offers = client
        .get_component::<Menus>()
        .and_then(|menus| menus.offers.clone())
        .filter(|offers| offers.container_id == window.id);
    Ok((window, offers))
}

/// The price the payment slot will take: the base count with demand and the player's reputation
/// applied, as the client works it out from the offer.
fn cost_a(offer: &MerchantOffer) -> ItemStack {
    let base = offer.base_cost_a.count;
    let demand = ((base * offer.demand) as f32 * offer.price_multiplier).floor().max(0.0) as i32;
    let count = (base + demand + offer.special_price_diff).clamp(1, offer.base_cost_a.item.max_stack_size());
    let mut stack = cost(&offer.base_cost_a);
    if let ItemStack::Present(data) = &mut stack {
        data.count = count;
    }
    stack
}

fn cost(item: &ItemCost) -> ItemStack {
    ItemStack::from(item.clone().into_item_stack())
}

/// A client marks an offer out of stock by using it up, whichever way the server said so.
fn out_of_stock(offer: &MerchantOffer) -> bool {
    offer.out_of_stock || offer.uses >= offer.max_uses
}

fn uses(offer: &MerchantOffer) -> i32 {
    if offer.out_of_stock { offer.max_uses } else { offer.uses }
}

fn describe(offer: &MerchantOffer, number: usize, known: Option<&Known>) -> Value {
    json!({
        "number": number,
        "costA": stacks::held(&cost_a(offer)),
        "baseCountA": offer.base_cost_a.count,
        "costB": offer.cost_b.as_ref().map_or(Value::Null, |item| stacks::held(&cost(item))),
        "result": stacks::held(&offer.result),
        "enchantments": enchantments(&offer.result, known),
        "uses": uses(offer),
        "maxUses": offer.max_uses,
        "outOfStock": out_of_stock(offer),
        "xp": offer.xp,
    })
}

/// A librarian sells a dozen enchanted books that read "enchanted_book x1" alike; which book is the
/// whole trade. A book stores them apart from what an enchanted tool carries, so both.
fn enchantments(stack: &ItemStack, known: Option<&Known>) -> Vec<Value> {
    let stored = stack
        .get_component::<StoredEnchantments>()
        .map(|stored| stored.enchantments.clone());
    let carried = stack
        .get_component::<Enchantments>()
        .map(|carried| carried.levels.clone());

    stored
        .into_iter()
        .chain(carried)
        .flatten()
        .map(|(enchantment, level)| {
            let name = known
                .and_then(|known| known.entry("minecraft:enchantment", enchantment.protocol_id() as usize))
                .map_or_else(|| "unknown".to_owned(), |(id, _)| id.path().to_owned());
            json!({"name": name, "level": level})
        })
        .collect()
}

pub const READ_TRADES: Tool = Tool {
    name: "read-trades",
    run: |bot, _args| {
        Box::pin(async move {
            let data = in_world(&bot, |game| {
                let (window, offers) = require(&game.client)?;
                let known = game.client.get_component::<Known>();
                let trades: Vec<Value> = offers
                    .as_ref()
                    .map(|offers| {
                        offers
                            .offers
                            .iter()
                            .enumerate()
                            .map(|(index, offer)| describe(offer, index + 1, known.as_deref()))
                            .collect()
                    })
                    .unwrap_or_default();

                /*
                A wandering trader is sent a level too, and the screen hides it because the progress
                bar is off. Both go, so the server can do what the screen does.
                */
                Ok::<_, Failure>(json!({
                    "title": window.title.to_string(),
                    "titleComponent": component(&window.title),
                    "level": offers.as_ref().map_or(0, |offers| offers.villager_level),
                    "xp": offers.as_ref().map_or(0, |offers| offers.villager_xp),
                    "showProgressBar": offers.as_ref().is_some_and(|offers| offers.show_progress),
                    "canRestock": offers.as_ref().is_some_and(|offers| offers.can_restock),
                    "trades": trades,
                }))
            })??;
            Ok(Answer::data("read-trades", data))
        })
    },
};

/// Pick a trade the way pressing it in the list does. Picking is not trading: the server moves the
/// price into the payment slots and fills the result from them, and the slots read afterwards say
/// whether it could. The trade itself is taking slot 2 with click-slot.
pub const SELECT_TRADE: Tool = Tool {
    name: "select-trade",
    run: |bot, args| {
        Box::pin(async move {
            let query = text(&args, "trade")?.trim().to_owned();

            let (window, offer, number) = alive(&bot, |game| {
                let (window, offers) = require(&game.client)?;
                let offers = offers.map(|offers| offers.offers).unwrap_or_default();
                if offers.is_empty() {
                    return Err(Failure::refused(
                        "NO_TRADES_YET",
                        "the trading screen lists no trades yet. They arrive a moment after it opens: wait a few ticks and read-trades.",
                    ));
                }
                let index = pick(&offers, &query)?;
                game.client.write_packet(ServerboundSelectTrade { item: index as u32 });
                Ok((window, offers[index].clone(), index + 1))
            })??;

            resync(&bot, window.id).await?;

            let data = in_world(&bot, |game| {
                let known = game.client.get_component::<Known>();
                let slots = menu(&game.client).map(|window| window.menu.slots()).unwrap_or_default();
                let slot = |index: usize| slots.get(index).map_or(Value::Null, stacks::held);
                json!({
                    "trade": describe(&offer, number, known.as_deref()),
                    "paymentA": slot(PAYMENT_A),
                    "paymentB": slot(PAYMENT_B),
                    "result": slot(RESULT),
                })
            })?;
            Ok(Answer::data("select-trade", data))
        })
    },
};

/// A number is the trade's place in the list. Anything else names what the trade gives, exactly
/// first: a villager that buys wheat, potatoes and carrots gives emeralds for all three, and
/// "emerald" picking whichever came first would be a trade nobody asked for.
fn pick(offers: &[MerchantOffer], query: &str) -> Result<usize, Failure> {
    if query.len() <= 3 && !query.is_empty() && query.bytes().all(|byte| byte.is_ascii_digit()) {
        let number: usize = query.parse().unwrap_or_default();
        if number < 1 || number > offers.len() {
            return Err(Failure::refused(
                "NO_SUCH_TRADE",
                format!(
                    "there is no trade {number}; this screen lists {}, numbered from 1.",
                    offers.len()
                ),
            ));
        }
        return Ok(number - 1);
    }

    let wanted = stacks::needle(query);
    let (mut exact, mut partial) = (Vec::new(), Vec::new());
    for (index, offer) in offers.iter().enumerate() {
        if stacks::name(&offer.result) == wanted || stacks::shown_as(&offer.result).eq_ignore_ascii_case(query) {
            exact.push(index);
        } else if stacks::matches(&offer.result, query) {
            partial.push(index);
        }
    }

    let found = if exact.is_empty() { partial } else { exact };
    match found.as_slice() {
        [only] => Ok(*only),
        [] => Err(Failure::refused(
            "NO_SUCH_TRADE",
            format!(
                "no trade gives \"{query}\". This screen sells {}.",
                list(offers, &(0..offers.len()).collect::<Vec<_>>())
            ),
        )),
        many => Err(Failure::refused(
            "AMBIGUOUS_TRADE",
            format!(
                "{} trades give \"{query}\": {}. Pick one by its number.",
                many.len(),
                list(offers, many)
            ),
        )),
    }
}

fn list(offers: &[MerchantOffer], indices: &[usize]) -> String {
    indices
        .iter()
        .map(|index| {
            let result = &offers[*index].result;
            format!("{}. {} x{}", index + 1, stacks::shown_as(result), result.count())
        })
        .collect::<Vec<_>>()
        .join(", ")
}
