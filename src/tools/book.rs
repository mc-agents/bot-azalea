use azalea::entity::inventory::Inventory;
use azalea::inventory::components::{WritableBookContent, WrittenBookContent};
use azalea::inventory::{ItemStack, Menu};
use azalea::protocol::packets::game::s_interact::InteractionHand;
use azalea::{Client, FormattedText};
use serde_json::{Value, json};

use super::text::component;
use super::windows::{menu, screen};
use super::{Tool, in_world};
use crate::calls::{Answer, Failure};
use crate::menus::Menus;

/// Read the book that is open, every page of it: a quest log, a rulebook, a guide handed out on
/// join. The client draws one page at a time, and a page is a component a picture cannot give back.
///
/// A lectern shows the same screen, so a book on a stand reads the same way as one in hand.
pub const READ_BOOK: Tool = Tool {
    name: "read-book",
    run: |bot, _args| {
        Box::pin(async move {
            let data = in_world(&bot, |game| open_book(&game.client))??;
            Ok(Answer::data("read-book", data))
        })
    },
};

fn open_book(client: &Client) -> Result<Value, Failure> {
    let window = menu(client);

    let (source, book, page) = match &window {
        Some(window) if matches!(window.menu, Menu::Lectern { .. }) => {
            let page = client
                .get_component::<Menus>()
                .and_then(|menus| menus.value(window.id, 0))
                .unwrap_or(0);
            ("lectern", window.menu.slot(0).cloned().unwrap_or_default(), page + 1)
        }
        _ => {
            let hand = client.get_component::<Menus>().and_then(|menus| menus.book);
            let Some(hand) = hand else {
                return Err(Failure::refused(
                    "NO_BOOK",
                    match &window {
                        None => {
                            "no book is open. Use a written book, or right-click a lectern with one on it.".to_owned()
                        }
                        Some(window) => format!("{} is open, and it is not a book.", screen(&window.menu)),
                    },
                ));
            };
            /* The screen always opens a book at its first page. */
            ("hand", held(client, hand), 1)
        }
    };

    let pages = pages(&book);
    let mut data = json!({
        "source": source,
        "page": page,
        "pages": pages.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "pageComponents": pages.iter().map(component).collect::<Vec<_>>(),
        "title": null,
        "author": null,
        "generation": null,
    });

    /*
    Title and author, which the screen does not hold: it is handed the pages alone. A lectern knows
    the stack it shows, and a book read from the hand is whichever hand holds a written one.
    */
    let written = if source == "lectern" {
        book.get_component::<WrittenBookContent>()
            .map(|content| content.into_owned())
    } else {
        [InteractionHand::MainHand, InteractionHand::OffHand]
            .into_iter()
            .find_map(|hand| {
                held(client, hand)
                    .get_component::<WrittenBookContent>()
                    .map(|content| content.into_owned())
            })
    };
    if let Some(written) = written {
        data["title"] = json!(written.title.raw);
        data["author"] = json!(written.author);
        data["generation"] = json!(written.generation);
    }
    Ok(data)
}

fn held(client: &Client, hand: InteractionHand) -> ItemStack {
    let Some(inventory) = client.get_component::<Inventory>() else {
        return ItemStack::Empty;
    };
    match hand {
        InteractionHand::MainHand => inventory.held_item().clone(),
        InteractionHand::OffHand => inventory.inventory_menu.as_player().offhand.clone(),
    }
}

/// A written book's pages as the server wrote them, or a writable book's as the text typed into it.
fn pages(book: &ItemStack) -> Vec<FormattedText> {
    if let Some(written) = book.get_component::<WrittenBookContent>() {
        return written.pages.iter().map(|page| page.raw.clone()).collect();
    }
    book.get_component::<WritableBookContent>()
        .map(|writable| {
            writable
                .pages
                .iter()
                .map(|page| FormattedText::from(page.raw.clone()))
                .collect()
        })
        .unwrap_or_default()
}
