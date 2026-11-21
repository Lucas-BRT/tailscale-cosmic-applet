// SPDX-License-Identifier: GPL-3.0-or-later

#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::elidable_lifetime_names,
    clippy::if_not_else,
    clippy::items_after_statements,
    clippy::match_same_arms,
    clippy::needless_pass_by_value,
    clippy::too_many_lines,
    clippy::unused_self,
    reason = "pre-restructuring monolithic app.rs; remove as modules are extracted"
)]
#[cfg_attr(
    test,
    expect(
        clippy::assert_is_empty,
        clippy::float_cmp,
        reason = "pre-restructuring monolithic app.rs tests; remove as modules are extracted"
    )
)]
mod app;
mod i18n;

fn main() -> cosmic::iced::Result {
    let requested_languages = i18n_embed::DesktopLanguageRequester::requested_languages();
    i18n::init(&requested_languages);
    cosmic::applet::run::<app::AppModel>(())
}
