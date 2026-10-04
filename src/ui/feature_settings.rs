//! Native feature settings, compiled inside Spotty's settings window module.
//! The parent provides shared settings widgets and config persistence.
use super::*;

/// Hours paired with the cadence labels (same index).
const INTERVAL_HOURS: [u32; 5] = [1, 6, 12, 24, 168];

/// Fraction-digit choices offered in the calculator popup (same index).
const CALC_PRECISIONS: [u32; 5] = [2, 4, 6, 8, 10];

/// Open a result type's settings popup.
pub(super) fn open_result_dialog(id: &str, parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    match id {
        "apps" => open_apps_dialog(parent, config),
        "newapps" => open_new_apps_dialog(parent, config),
        "web" => open_web_dialog(parent, config),
        "calc" => open_calc_dialog(parent, config),
        "convert" => open_convert_dialog(parent, config),
        "updates" => open_updates_dialog(parent, config),
        _ => {}
    }
}

/// The package-manager choice behind "Search for new apps", as a popup
/// (it used to be a standalone section).
fn open_new_apps_dialog(parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    // Kick the source probes (idempotent, cached) so availability answers
    // are as fresh as possible before building the row list.
    crate::search::cmd::preload_install_cache_async(config.borrow().app_sources());
    let dlg = adw::PreferencesDialog::builder()
        .title(gettext("Search for new Apps"))
        .content_width(440)
        .build();
    let page = adw::PreferencesPage::builder().build();
    let g = adw::PreferencesGroup::builder()
        .title(gettext("App Sources"))
        .build();

    // One switch per source. The probes are asynchronous: a source whose
    // probe hasn't answered yet shows as a disabled "Checking…" row (instead
    // of silently missing) and the rows update in place as answers arrive.
    let flatpak_row = source_switch_row(
        &g,
        config,
        &gettext("Flatpak"),
        &gettext("Search and install Flatpak apps"),
        |s| s.flatpak,
        |c, v| c.pm_flatpak = Some(v),
    );
    let distro_row = source_switch_row(
        &g,
        config,
        &gettext("System packages"),
        // Replaced by the sync pass below before the dialog is presented.
        &gettext("Checking…"),
        |s| s.distro,
        |c, v| c.pm_distro = Some(v),
    );
    let snap_row = source_switch_row(
        &g,
        config,
        &gettext("Snap"),
        &gettext("Search and install Snap packages"),
        |s| s.snap,
        |c, v| c.pm_snap = Some(v),
    );
    let appimage_row = source_switch_row(
        &g,
        config,
        &gettext("AppImages"),
        &gettext("Search, launch and remove AppImages"),
        |s| s.appimage,
        |c, v| c.pm_appimage = Some(v),
    );
    let none_row = adw::ActionRow::builder()
        .title(gettext("No app sources available"))
        .subtitle(gettext("No Flatpak, package manager, Snap or AppImage found"))
        .use_markup(false)
        .build();
    g.add(&none_row);

    // Apply one probe answer to its row: available → enabled with the real
    // subtitle, pending → disabled "Checking…", missing → hidden.
    fn apply_source_row(row: &adw::SwitchRow, avail: Option<bool>, ok_subtitle: &str) {
        match avail {
            Some(true) => {
                row.set_subtitle(ok_subtitle);
                row.set_sensitive(true);
                row.set_visible(true);
            }
            None => {
                row.set_subtitle(&gettext("Checking…"));
                row.set_sensitive(false);
                row.set_visible(true);
            }
            Some(false) => row.set_visible(false),
        }
    }

    // Re-read the four probes and update the rows. Returns true once every
    // probe has answered (the poll can stop).
    let sync: Rc<dyn Fn() -> bool> = {
        let flatpak_row = flatpak_row.clone();
        let distro_row = distro_row.clone();
        let snap_row = snap_row.clone();
        let appimage_row = appimage_row.clone();
        let none_row = none_row.clone();
        Rc::new(move || {
            let flatpak = crate::search::cmd::flatpak_is_available();
            let snap = crate::search::cmd::snap_is_available();
            let appimage = crate::search::appimage::is_supported();
            use crate::search::cmd::SystemPackages;
            // What this system is, and what it lets Spotty do with packages.
            let status = crate::search::cmd::system_packages();
            let distro = match &status {
                // Known but unusable stays visible (greyed, with the reason),
                // so it is clear why there is nothing to switch on.
                SystemPackages::Ready { .. }
                | SystemPackages::Immutable { .. }
                | SystemPackages::NoService { .. } => Some(true),
                SystemPackages::Unsupported { .. } => Some(false),
            };
            apply_source_row(
                &flatpak_row,
                flatpak,
                &gettext("Search and install Flatpak apps"),
            );
            match &status {
                SystemPackages::Ready { distro: name, pm, via } => {
                    apply_source_row(
                        &distro_row,
                        distro,
                        &gettext("Search, install and update system packages — {distro} ({pm}, {via})")
                            .replace("{distro}", name)
                            .replace("{pm}", pm)
                            .replace("{via}", via),
                    );
                }
                SystemPackages::Immutable { distro: name } => {
                    apply_source_row(&distro_row, distro, "");
                    distro_row.set_active(false);
                    distro_row.set_sensitive(false);
                    distro_row.set_subtitle(
                        &gettext("{distro} is image-based — system packages aren't changed here, use Flatpak")
                            .replace("{distro}", name),
                    );
                }
                SystemPackages::NoService { distro: name, pm } => {
                    apply_source_row(&distro_row, distro, "");
                    distro_row.set_sensitive(false);
                    distro_row.set_subtitle(
                        &gettext("{distro} ({pm}) needs PackageKit to install and update packages")
                            .replace("{distro}", name)
                            .replace("{pm}", pm),
                    );
                }
                SystemPackages::Unsupported { .. } => {
                    apply_source_row(&distro_row, distro, "");
                }
            }
            apply_source_row(&snap_row, snap, &gettext("Search and install Snap packages"));
            apply_source_row(
                &appimage_row,
                appimage,
                &gettext("Search, launch and remove AppImages"),
            );
            let any_ok = flatpak == Some(true)
                || distro == Some(true)
                || snap == Some(true)
                || appimage == Some(true);
            let all_answered =
                flatpak.is_some() && distro.is_some() && snap.is_some() && appimage.is_some();
            none_row.set_visible(all_answered && !any_ok);
            all_answered
        })
    };

    if !sync() {
        // A probe is still running: poll briefly so the rows fill in without
        // the user having to close and reopen the popup. Stops when every
        // probe has answered, on dialog close, or after ~18 s as a cap.
        let alive = Rc::new(std::cell::Cell::new(true));
        let a = alive.clone();
        dlg.connect_closed(move |_| a.set(false));
        let mut ticks = 0u32;
        glib::timeout_add_local(std::time::Duration::from_millis(300), move || {
            if !alive.get() || ticks >= 60 {
                return glib::ControlFlow::Break;
            }
            ticks += 1;
            if sync() {
                glib::ControlFlow::Break
            } else {
                glib::ControlFlow::Continue
            }
        });
    }

    page.add(&g);
    add_trigger_sections(&page, &dlg, config, "newapps");
    dlg.add(&page);
    dlg.present(Some(parent));
}

/// One availability-gated source switch: reads the current value out of
/// the config (`get`), writes every change back (`set`), saves on toggle.
fn source_switch_row(
    group: &adw::PreferencesGroup,
    config: &Rc<RefCell<Config>>,
    title: &str,
    subtitle: &str,
    get: fn(&crate::config::AppSources) -> bool,
    set: fn(&mut Config, bool),
) -> adw::SwitchRow {
    let active = get(&config.borrow().app_sources());
    let row = adw::SwitchRow::builder()
        .title(title)
        .subtitle(subtitle)
        .active(active)
        .use_markup(false)
        .build();
    {
        let cfg = config.clone();
        row.connect_active_notify(move |r| {
            let mut c = cfg.borrow_mut();
            set(&mut c, r.is_active());
            c.save();
        });
    }
    group.add(&row);
    row
}

/// The web-search settings as a popup: the engine choice behind the Web
/// Search row (the Custom Search URL only matters for the Custom engine).
/// Built fresh on every open so it always shows the current state.
fn open_web_dialog(parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    let dlg = adw::PreferencesDialog::builder()
        .title(gettext("Web Search"))
        .content_width(440)
        .build();
    let page = adw::PreferencesPage::builder().build();
    let g = adw::PreferencesGroup::new();

    // The Custom Search URL only makes sense for the Custom engine — the
    // row is hidden entirely for everything else.
    let custom_web = adw::EntryRow::builder()
        .title(gettext("Custom Search URL"))
        .show_apply_button(true)
        .use_markup(false)
        .build();
    custom_web.set_text(&config.borrow().custom_web_search_url);
    custom_web.set_tooltip_text(Some(
        "Use {query} as the placeholder, for example https://example.com/search?q={query}",
    ));
    {
        let cfg = config.clone();
        custom_web.connect_apply(move |r| {
            let mut c = cfg.borrow_mut();
            c.custom_web_search_url = r.text().trim().to_string();
            c.save();
        });
    }

    let er = adw::ComboRow::builder()
        .title(gettext("Search Engine"))
        .subtitle(gettext("Used for web search results"))
        .use_markup(false)
        .build();
    er.set_model(Some(&gtk::StringList::new(
        &SearchEngine::all()
            .iter()
            .map(|e| e.display_name())
            .collect::<Vec<_>>(),
    )));
    if let Some(i) = SearchEngine::all()
        .iter()
        .position(|&e| e == config.borrow().search_engine)
    {
        er.set_selected(i as u32);
    }
    let cfg = config.clone();
    let custom_web_vis = custom_web.clone();
    let is_custom = |e: SearchEngine| e == SearchEngine::Custom;
    custom_web_vis.set_visible(SearchEngine::all()
        .get(er.selected() as usize)
        .copied()
        .map(is_custom)
        .unwrap_or(false));
    er.connect_selected_notify(move |r| {
        if let Some(&e) = SearchEngine::all().get(r.selected() as usize) {
            let mut c = cfg.borrow_mut();
            c.search_engine = e;
            c.save();
            custom_web_vis.set_visible(is_custom(e));
        }
    });
    g.add(&er);
    g.add(&custom_web);
    page.add(&g);

    // Opening the search privately is part of searching, so its shortcut lives
    // here next to the engine — edited with the same "Set shortcut…" dialog as
    // every other shortcut.
    let shortcuts = adw::PreferencesGroup::builder()
        .title(gettext("On a Web Result"))
        .build();
    {
        let default = Config::default().private_search_shortcut;
        let current = shortcut_value(config, "private_search", &default);
        let cell: Rc<RefCell<String>> = Rc::new(RefCell::new(current.clone()));
        let cfg_k = config.clone();
        let field_k = "private_search".to_string();
        let cell_k = cell.clone();
        let (row, _) = capture_shortcut_row(
            &dlg,
            &gettext("Search privately"),
            "",
            cell,
            Rc::new(move || {
                let mut c = cfg_k.borrow_mut();
                set_shortcut(&mut c, &field_k, &cell_k.borrow());
                c.save();
            }),
            &default,
        );
        shortcuts.add(&row);
        page.add(&shortcuts);
    }

    add_trigger_sections(&page, &dlg, config, "web");
    dlg.add(&page);
    dlg.present(Some(parent));
}

/// The application-search settings as a popup: the shortcuts that act on
/// a selected app in the results (uninstall / kill), adjustable right
/// where the app results are toggled — the same fields the Open Spotty
/// dialog exposes, so both stay in sync through the config.
fn open_apps_dialog(parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    let dlg = adw::PreferencesDialog::builder()
        .title(gettext("Applications"))
        .content_width(440)
        .build();
    let page = adw::PreferencesPage::builder().build();
    let g = adw::PreferencesGroup::builder()
        .title(gettext("On an App Result"))
        .build();

    for (title, field, default) in [
        (gettext("Uninstall app"), "uninstall", "<Control>u"),
        (gettext("Kill app"), "kill", "<Control>k"),
    ] {
        let current = shortcut_value(config, field, default);
        let cell: Rc<RefCell<String>> = Rc::new(RefCell::new(current.clone()));
        let cfg_k = config.clone();
        let field_k = field.to_string();
        let cell_k = cell.clone();
        let (row, _) = capture_shortcut_row(
            &dlg,
            &title,
            "",
            cell,
            Rc::new(move || {
                let mut c = cfg_k.borrow_mut();
                set_shortcut(&mut c, &field_k, &cell_k.borrow());
                c.save();
            }),
            default,
        );
        g.add(&row);
    }
    page.add(&g);
    add_trigger_sections(&page, &dlg, config, "apps");
    dlg.add(&page);
    dlg.present(Some(parent));
}

/// The calculator settings as a popup: how answers are shaped.
fn open_calc_dialog(parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    let dlg = adw::PreferencesDialog::builder()
        .title(gettext("Calculator"))
        .content_width(440)
        .build();
    let page = adw::PreferencesPage::builder().build();

    // Arithmetic: how answers are shaped.
    {
        let g = adw::PreferencesGroup::builder()
            .title(gettext("Arithmetic"))
            .build();

        // Decimal places: 2/4/6/8/10 — the labels are digits, no translation.
        {
            let row = adw::ComboRow::builder()
                .title(gettext("Decimal places"))
                .use_markup(false)
                .build();
            row.set_model(Some(&gtk::StringList::new(&["2", "4", "6", "8", "10"])));
            row.set_selected(
                CALC_PRECISIONS
                    .iter()
                    .position(|p| *p == config.borrow().calc_precision)
                    .unwrap_or(2) as u32,
            );
            let cfg = config.clone();
            row.connect_selected_notify(move |r| {
                let p = CALC_PRECISIONS
                    .get(r.selected() as usize)
                    .copied()
                    .unwrap_or(6);
                let mut c = cfg.borrow_mut();
                c.calc_precision = p;
                c.save();
            });
            g.add(&row);
        }

        for (title, get, set) in [
            (
                gettext("Thousands separators"),
                (|c: &Config| c.calc_separators) as fn(&Config) -> bool,
                (|c, v| c.calc_separators = v) as fn(&mut Config, bool),
            ),
            (
                gettext("Show hex, octal and binary"),
                |c| c.calc_bases,
                |c, v| c.calc_bases = v,
            ),
            (
                gettext("Show the expression"),
                |c| c.calc_show_expr,
                |c, v| c.calc_show_expr = v,
            ),
            (
                gettext("Paste the result automatically"),
                |c| c.calc_paste,
                |c, v| c.calc_paste = v,
            ),
            (
                gettext("Scientific notation"),
                |c| c.calc_sci_notation,
                |c, v| c.calc_sci_notation = v,
            ),
        ] {
            add_calc_toggle(&g, config, &title, get, set);
        }
        page.add(&g);
    }

    add_trigger_sections(&page, &dlg, config, "calc");
    dlg.add(&page);
    dlg.present(Some(parent));
}

/// The converter settings as a popup: what it converts, how it is worded and
/// what a conversion without a target answers with.
fn open_convert_dialog(parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    let dlg = adw::PreferencesDialog::builder()
        .title(gettext("Converter"))
        .content_width(440)
        .build();
    let page = adw::PreferencesPage::builder().build();

    // Conversions: what else the search box can answer.
    {
        let g = adw::PreferencesGroup::builder()
            .title(gettext("Conversions"))
            .description(gettext("Rates are fetched from a free public API when you convert"))
            .build();
        for (title, get, set) in [
            (
                gettext("Unit conversions"),
                (|c: &Config| c.calc_converter) as fn(&Config) -> bool,
                (|c, v| c.calc_converter = v) as fn(&mut Config, bool),
            ),
            (
                gettext("Show equivalents when no target unit is given"),
                |c| c.calc_equivalents,
                |c, v| c.calc_equivalents = v,
            ),
            (
                gettext("Number base conversions"),
                |c| c.calc_base_convert,
                |c, v| c.calc_base_convert = v,
            ),
            (
                gettext("Currency conversion"),
                |c| c.calc_currency,
                |c, v| c.calc_currency = v,
            ),
        ] {
            add_calc_toggle(&g, config, &title, get, set);
        }

        // Wording: the connector the converter understands — presets with a
        // live example, plus a free-text custom entry.
        {
            let row = adw::ComboRow::builder()
                .title(gettext("Conversion wording"))
                .use_markup(false)
                .build();
            let labels = [
                gettext("to"),
                gettext("to or in"),
                gettext("in"),
                "\u{2192}".to_string(),
                "=".to_string(),
                gettext("Comma"),
                gettext("None (space only)"),
                gettext("Custom…"),
            ];
            let model = gtk::StringList::new(&[]);
            for l in &labels {
                model.append(l);
            }
            row.set_model(Some(&model));
            const PRESETS: [&[&str]; 7] = [
                &["to"],
                &["to", "in"],
                &["in"],
                &["\u{2192}"],
                &["="],
                &[","],
                &[],
            ];
            let words_of_entry = |text: &str| -> Vec<String> {
                text.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect()
            };
            let example = |words: &[String]| -> String {
                if words.is_empty() {
                    "20 USD EUR".to_string()
                } else {
                    format!("20 USD {} EUR", words.join(" / "))
                }
            };
            let current: Vec<String> = config.borrow().calc_convert_words.clone();
            let sel = PRESETS
                .iter()
                .position(|p| {
                    p.len() == current.len()
                        && p.iter().zip(current.iter()).all(|(a, b)| *a == b.as_str())
                })
                .unwrap_or(PRESETS.len());
            row.set_selected(sel as u32);
            row.set_subtitle(
                &gettext("Example: {example}").replace("{example}", &example(&current)),
            );
            let entry = adw::EntryRow::builder()
                .title(gettext("Custom wording"))
                .text(current.join(", "))
                .visible(sel == PRESETS.len())
                .use_markup(false)
                .build();
            g.add(&row);
            g.add(&entry);
            {
                let cfg = config.clone();
                let entry = entry.clone();
                row.connect_selected_notify(move |r| {
                    let idx = r.selected() as usize;
                    let words: Vec<String> = if idx < PRESETS.len() {
                        PRESETS[idx].iter().map(|s| (*s).to_string()).collect()
                    } else {
                        words_of_entry(&entry.text())
                    };
                    entry.set_visible(idx >= PRESETS.len());
                    if idx < PRESETS.len() {
                        entry.set_text(&words.join(", "));
                    }
                    r.set_subtitle(
                        &gettext("Example: {example}").replace("{example}", &example(&words)),
                    );
                    let mut c = cfg.borrow_mut();
                    c.calc_convert_words = words;
                    c.save();
                });
            }
            {
                let cfg = config.clone();
                let row = row.clone();
                entry.connect_changed(move |e| {
                    let words = words_of_entry(&e.text());
                    row.set_subtitle(
                        &gettext("Example: {example}").replace("{example}", &example(&words)),
                    );
                    let mut c = cfg.borrow_mut();
                    c.calc_convert_words = words;
                    c.save();
                });
            }
        }

        // Default currency: what a source-less conversion aims at
        // ("20 EUR" -> the picked currency; the source itself falls back
        // to the automatic pick, so "20 USD" under a USD default still
        // answers in EUR).
        {
            let codes = [
                "usd", "eur", "gbp", "try", "jpy", "cny", "inr", "chf", "cad", "aud",
            ];
            let row = adw::ComboRow::builder()
                .title(gettext("Default currency"))
                .use_markup(false)
                .build();
            let model = gtk::StringList::new(&[]);
            for c in codes {
                model.append(&c.to_uppercase());
            }
            row.set_model(Some(&model));
            let example_of = |code: &str| -> String {
                gettext("Example: {example}")
                    .replace("{example}", &format!("20 EUR \u{2192} {}", code.to_uppercase()))
            };
            let sel = codes
                .iter()
                .position(|c| *c == config.borrow().calc_default_currency.to_lowercase())
                .unwrap_or(0);
            row.set_selected(sel as u32);
            row.set_subtitle(&example_of(codes.get(sel).copied().unwrap_or("usd")));
            {
                let cfg = config.clone();
                row.connect_selected_notify(move |r| {
                    let code = codes.get(r.selected() as usize).copied().unwrap_or("usd");
                    r.set_subtitle(&example_of(code));
                    let mut c = cfg.borrow_mut();
                    c.calc_default_currency = code.to_string();
                    c.save();
                });
            }
            g.add(&row);
        }
        page.add(&g);
    }

    // Default targets: what a no-target conversion answers with, per
    // dimension — the automatic pick unless the user fixes one.
    {
        let g = adw::PreferencesGroup::builder()
            .title(gettext("Default targets"))
            .description(gettext("What a value converts to when you don't name a target"))
            .build();
        let titles = [
            gettext("Length"),
            gettext("Mass"),
            gettext("Temperature"),
            gettext("Volume"),
            gettext("Area"),
            gettext("Speed"),
            gettext("Time"),
            gettext("Data"),
            gettext("Pressure"),
            gettext("Energy"),
            gettext("Power"),
            gettext("Force"),
            gettext("Angle"),
            gettext("Frequency"),
            gettext("Fuel economy"),
        ];
        let auto = gettext("Auto");
        for (i, (key, syms)) in crate::search::convert::default_target_dims()
            .into_iter()
            .enumerate()
        {
            let title = titles.get(i).cloned().unwrap_or_else(|| key.to_string());
            let row = adw::ComboRow::builder().title(&title).use_markup(false).build();
            let mut opts: Vec<String> = vec![auto.clone()];
            for s in syms {
                let s = s.to_string();
                if !opts.contains(&s) {
                    opts.push(s);
                }
            }
            let model = gtk::StringList::new(&[]);
            for o in &opts {
                model.append(o);
            }
            row.set_model(Some(&model));
            let sel = config
                .borrow()
                .calc_default_targets
                .get(key)
                .and_then(|v| opts.iter().position(|o| o == v))
                .unwrap_or(0);
            row.set_selected(sel as u32);
            {
                let cfg = config.clone();
                row.connect_selected_notify(move |r| {
                    let idx = r.selected() as usize;
                    let mut c = cfg.borrow_mut();
                    if idx == 0 {
                        c.calc_default_targets.remove(key);
                    } else if let Some(sym) = opts.get(idx) {
                        c.calc_default_targets.insert(key.to_string(), sym.clone());
                    }
                    c.save();
                });
            }
            g.add(&row);
        }
        page.add(&g);
    }

    add_trigger_sections(&page, &dlg, config, "convert");
    dlg.add(&page);
    dlg.present(Some(parent));
}

/// One on/off row wired straight to a `Config` bool, saved on change —
/// the shape every toggle in the Calculator & Converter dialog uses.
fn add_calc_toggle(
    g: &adw::PreferencesGroup,
    config: &Rc<RefCell<Config>>,
    title: &str,
    get: fn(&Config) -> bool,
    set: fn(&mut Config, bool),
) {
    let row = adw::SwitchRow::builder()
        .title(title)
        .active(get(&config.borrow()))
        .use_markup(false)
        .build();
    let cfg = config.clone();
    row.connect_active_notify(move |r| {
        let active = r.is_active();
        let mut c = cfg.borrow_mut();
        set(&mut c, active);
        c.save();
    });
    g.add(&row);
}

/// The updates settings as a popup: check-now with its live status,
/// notification, cadence, notice controls and the restart row. Built fresh
/// on every open so it always shows the current state.
fn open_updates_dialog(parent: &adw::PreferencesWindow, config: &Rc<RefCell<Config>>) {
    let enabled = config.borrow().enable_updates;
    let dlg = adw::PreferencesDialog::builder()
        .title(gettext("Updates"))
        .content_width(440)
        .build();
    let page = adw::PreferencesPage::builder().build();

    // Check now: the status as its subtitle; the ↻ (or the row) re-checks.
    let g = adw::PreferencesGroup::builder().build();
    let check_row = adw::ActionRow::builder()
        .title(gettext("Check now"))
        .subtitle(&crate::search::cmd::update_status_text(enabled))
        .activatable(true)
        .use_markup(false)
        .build();
    let check_btn = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .css_classes(["flat"])
        .valign(gtk::Align::Center)
        .tooltip_text(gettext("Check now"))
        .build();
    check_btn.set_sensitive(enabled && !crate::search::cmd::updates_checking());
    check_row.add_suffix(&check_btn);
    let recheck: Rc<dyn Fn()> = {
        let row = check_row.clone();
        let btn = check_btn.clone();
        let cfg = config.clone();
        Rc::new(move || {
            crate::search::cmd::check_updates_now();
            let on = cfg.borrow().enable_updates;
            row.set_subtitle(&crate::search::cmd::update_status_text(on));
            btn.set_sensitive(false);
        })
    };
    {
        let f = recheck.clone();
        check_row.connect_activated(move |_| f());
    }
    {
        let f = recheck.clone();
        check_btn.connect_clicked(move |_| f());
    }
    g.add(&check_row);

    {
        let row = adw::SwitchRow::builder()
            .title(gettext("Update notification"))
            .subtitle(gettext("Show a badge and a desktop notification when updates exist"))
            .active(config.borrow().update_notification)
            .use_markup(false)
            .build();
        let cfg = config.clone();
        row.connect_active_notify(move |r| {
            let active = r.is_active();
            save_and_refresh(&cfg, |c| c.update_notification = active);
        });
        g.add(&row);
    }
    {
        let row = adw::ComboRow::builder()
            .title(gettext("Check every"))
            .use_markup(false)
            .build();
        let intervals_model = gtk::StringList::new(&[]);
        // Literal gettext calls so the labels land in the translation
        // catalogue (a variable-driven gettext() can't be extracted).
        for label in [
            gettext("Hourly"),
            gettext("Every 6 hours"),
            gettext("Every 12 hours"),
            gettext("Daily"),
            gettext("Weekly"),
        ] {
            intervals_model.append(&label);
        }
        row.set_model(Some(&intervals_model));
        let cur = config.borrow().update_check_interval_hours;
        row.set_selected(
            INTERVAL_HOURS
                .iter()
                .position(|h| *h == cur)
                .unwrap_or(3) as u32,
        );
        let cfg = config.clone();
        row.connect_selected_notify(move |r| {
            if let Some(hours) = INTERVAL_HOURS.get(r.selected() as usize) {
                let hours = *hours;
                save_and_refresh(&cfg, |c| c.update_check_interval_hours = hours);
            }
        });
        g.add(&row);
    }
    // The settings only mean something while the master switch is on.
    g.set_sensitive(enabled);
    page.add(&g);

    if crate::search::cmd::updates_pending() {
        let g2 = adw::PreferencesGroup::builder().build();
        let row = adw::ActionRow::builder()
            .title(gettext("Remind tomorrow"))
            .subtitle(gettext("Hide the update notice for 24 hours"))
            .activatable(true)
            .use_markup(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("document-open-recent-symbolic"));
        row.connect_activated(|_| crate::search::cmd::snooze_update_notice());
        g2.add(&row);

        let row = adw::ActionRow::builder()
            .title(gettext("Dismiss update notice"))
            .subtitle(gettext("Hidden until a new update appears"))
            .activatable(true)
            .use_markup(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("window-close-symbolic"));
        row.connect_activated(|_| crate::search::cmd::dismiss_current_update_notice());
        g2.add(&row);
        g2.set_sensitive(enabled);
        page.add(&g2);
    }

    // Offered only once the probe confirms a reboot is pending — i.e. after an
    // update run completed and left the machine needing a restart. Updates
    // themselves are never chained into this button: they run to completion
    // first, then this appears.
    if crate::search::cmd::reboot_pending() {
        let (title, subtitle) = crate::search::cmd::reboot_notice();
        let g3 = adw::PreferencesGroup::builder().build();
        let row = adw::ActionRow::builder()
            .title(title)
            .subtitle(subtitle)
            .activatable(true)
            .use_markup(false)
            .build();
        row.add_prefix(&gtk::Image::from_icon_name("system-reboot-symbolic"));
        let btn = gtk::Button::builder()
            .label(gettext("Restart"))
            .css_classes(["suggested-action"])
            .valign(gtk::Align::Center)
            .build();
        {
            let win = parent.clone();
            btn.connect_clicked(move |_| {
                let dialog = adw::MessageDialog::builder()
                    .transient_for(&win)
                    .heading(gettext("Are you sure?"))
                    .build();
                dialog.add_response("cancel", &gettext("Cancel"));
                dialog.add_response("confirm", &gettext("Confirm"));
                dialog.set_default_response(Some("confirm"));
                dialog.set_response_appearance("confirm", adw::ResponseAppearance::Destructive);
                let cmd = crate::search::system::reboot_command();
                dialog.connect_response(None, move |_, resp| {
                    if resp == "confirm" {
                        let _ = crate::app::spawn_host_shell_command(&cmd);
                    }
                });
                dialog.present();
            });
        }
        row.add_suffix(&btn);
        g3.add(&row);
        page.add(&g3);
    }

    add_trigger_sections(&page, &dlg, config, "updates");
    dlg.add(&page);

    // The status + ↻ keep refreshing while the popup is open, so a check
    // finishing behind it updates the row it was started from.
    {
        let alive = Rc::new(std::cell::Cell::new(true));
        let a2 = alive.clone();
        let row = check_row.clone();
        let btn = check_btn.clone();
        let cfg = config.clone();
        glib::timeout_add_local(std::time::Duration::from_secs(1), move || {
            if !a2.get() {
                return glib::ControlFlow::Break;
            }
            let on = cfg.borrow().enable_updates;
            row.set_subtitle(&crate::search::cmd::update_status_text(on));
            btn.set_sensitive(on && !crate::search::cmd::updates_checking());
            glib::ControlFlow::Continue
        });
        let a3 = alive.clone();
        dlg.connect_closed(move |_| a3.set(false));
    }
    dlg.present(Some(parent));
}

