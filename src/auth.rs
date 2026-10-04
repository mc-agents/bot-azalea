use std::time::Duration;

use azalea::account::Account;
use azalea::account::microsoft::MicrosoftAccountOpts;
use tracing::info;

use crate::config::{Auth, Microsoft};

/// How long the bot's own sign-in is given. Refreshing a cached credential is a handful of round
/// trips, to Microsoft, Xbox Live and Mojang, and takes seconds; a credential too old to refresh
/// drops azalea into the device flow instead, which polls for a quarter of an hour waiting for a
/// browser nobody is sitting in front of. The bound is what turns that wait into a message.
const SIGN_IN: Duration = Duration::from_secs(60);

/// The account every join is made with, or `None` offline, where the name a connect asks for is all
/// the identity there is.
///
/// Signed in here, before the link is dialled, and not at the first join: a credential that is
/// missing or past refreshing is a mistake in how the bot was started, and the one moment somebody
/// is reading the output of the command that started it is now. A bot that linked first would turn
/// that one mistake into a refusal on every connect, which reads as the game server's fault.
pub async fn sign_in(auth: &Auth) -> Result<Option<Account>, String> {
    let Auth::Microsoft(microsoft) = auth else {
        return Ok(None);
    };

    /*
    azalea answers a cache it cannot find by starting the device flow, which prints a link and then
    polls -- into a log nobody is attached to. The file is looked for here so that the usual
    mistake, a volume that was never logged in to or a BOT_AUTH_CACHE pointing somewhere other than
    where the login wrote, is one line instead of fifteen minutes of silence.
    */
    if !microsoft.cache.is_file() {
        return Err(format!(
            "BOT_AUTH is microsoft and there is no credential cache at {}: run this image once with the argument `login` to write one, with the same BOT_AUTH_ACCOUNT and BOT_AUTH_CACHE and a terminal attached",
            microsoft.cache.display()
        ));
    }

    let Ok(signed_in) = tokio::time::timeout(SIGN_IN, authenticate(microsoft)).await else {
        return Err(format!(
            "Microsoft authentication for {} got no answer in {}s. The cache is there, so either it holds no credential under this BOT_AUTH_ACCOUNT or the one it holds is too old to refresh; both drop azalea into the device flow, which prints a link and waits for a browser. Run this image with the argument `login` again, with a terminal attached",
            microsoft.account,
            SIGN_IN.as_secs()
        ));
    };
    let account = signed_in?;
    info!("authenticated with Microsoft as {}", account.username());
    Ok(Some(account))
}

/// Log in once with somebody watching, write the credential the bot will read, and say which name
/// the account turned out to be.
///
/// Unbounded, where the bot's sign-in is not: the whole point of this mode is that there is a human
/// at the other end of it to open the link azalea prints.
pub async fn login(microsoft: &Microsoft) -> Result<String, String> {
    info!("logging in as {}; the link to open follows", microsoft.account);

    let account = authenticate(microsoft).await?;

    /*
    Read back, because the write is not reported. azalea logs a cache it could not write and returns
    success anyway, so a login against a path it cannot create -- a directory where a file belongs,
    a volume mounted read-only, a mount owned by root -- ends by printing the name it signed in as
    while nothing was kept. The bot then refuses to start for want of the credential, and under a
    restart policy it says so for ever, telling somebody to do the thing they have just done.
    */
    if !microsoft.cache.is_file() {
        return Err(format!(
            "signed in as {}, and nothing was written to {}: azalea reports a cache it could not write as a success, so the path is what to look at -- it is a file and not a directory, and the volume it is on has to be writable by this image's uid",
            account.username(),
            microsoft.cache.display()
        ));
    }
    Ok(account.username().to_string())
}

/// The one call both modes make, so that the credential a login writes is by construction the one a
/// bot reads: with a cache to refresh from it answers without asking anybody anything, and without
/// one it prints a Microsoft link and waits.
async fn authenticate(microsoft: &Microsoft) -> Result<Account, String> {
    let opts = MicrosoftAccountOpts {
        // Off because it fails outright for an account that has the game through Game Pass, and the
        // profile this fetches anyway is what says whether the account can play.
        check_ownership: false,
        // Always ours. azalea's default is under a home directory the image's uid does not have,
        // and a cache that cannot be found is a device flow nobody will answer.
        cache_file: Some(microsoft.cache.clone()),
        client_id: None,
        scope: None,
    };
    Account::microsoft_with_opts(&microsoft.account, opts)
        .await
        .map_err(|error| format!("Microsoft authentication for {} failed: {error}", microsoft.account))
}
