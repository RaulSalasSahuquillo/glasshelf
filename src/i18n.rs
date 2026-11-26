//! Minimal localisation of the dock's own UI strings.
//!
//! English is the default; Spanish is used when the locale asks for it. Application
//! names and desktop actions come from the `.desktop` files, which are localised by
//! the applications themselves, so they are not handled here.
//!
//! The language is taken from, in order: `GLASSDOCK_LANG`, `LC_ALL`, `LC_MESSAGES`,
//! `LANG`. Anything that does not start with `es` falls back to English.

use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

/// Every string the dock draws by itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Msg {
    NewWindow,
    MinimizeOne,
    MinimizeAll,
    CloseOne,
    CloseAll,
    MoveLeft,
    MoveRight,
    Unpin,
    Pin,
    AddPrompt,
    SettingsMenu,
    ForgetRecent,
    SettingsTitle,
    SecAppearance,
    AutoHide,
    AutoHideHint,
    LiveGlass,
    LiveGlassHint,
    OutlineSystem,
    OutlineSystemHint,
    Magnification,
    MagnificationHint,
    Zoom,
    SecRecommended,
    ShowRecommended,
    ShowRecommendedHint,
    RecommendedCount,
    ClearHistory,
    HistoryCleared,
}

/// Picks a language from a locale string such as `es_ES.UTF-8`.
pub fn parse_lang(locale: &str) -> Lang {
    if locale.trim().to_ascii_lowercase().starts_with("es") {
        Lang::Es
    } else {
        Lang::En
    }
}

/// Detects the language from the environment.
pub fn detect() -> Lang {
    for var in ["GLASSDOCK_LANG", "LC_ALL", "LC_MESSAGES", "LANG"] {
        if let Ok(v) = std::env::var(var) {
            let v = v.trim();
            if !v.is_empty() && v != "C" && v != "POSIX" {
                return parse_lang(v);
            }
        }
    }
    Lang::En
}

/// The language of this process (detected once).
pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(detect)
}

/// Translates `msg` into the language of this process.
pub fn tr(msg: Msg) -> &'static str {
    translate(lang(), msg)
}

/// Translates `msg` into an explicit language.
pub fn translate(lang: Lang, msg: Msg) -> &'static str {
    match (lang, msg) {
        (Lang::En, Msg::NewWindow) => "New window",
        (Lang::En, Msg::MinimizeOne) => "Minimize",
        (Lang::En, Msg::MinimizeAll) => "Minimize all",
        (Lang::En, Msg::CloseOne) => "Close",
        (Lang::En, Msg::CloseAll) => "Close all windows",
        (Lang::En, Msg::MoveLeft) => "Move left",
        (Lang::En, Msg::MoveRight) => "Move right",
        (Lang::En, Msg::Unpin) => "Remove from dock",
        (Lang::En, Msg::Pin) => "Keep in dock",
        (Lang::En, Msg::AddPrompt) => "Add to dock",
        (Lang::En, Msg::SettingsMenu) => "Dock settings…",
        (Lang::En, Msg::ForgetRecent) => "Remove from recommendations",
        (Lang::En, Msg::SettingsTitle) => "GlassDock Settings",
        (Lang::En, Msg::SecAppearance) => "Appearance",
        (Lang::En, Msg::AutoHide) => "Hide automatically",
        (Lang::En, Msg::AutoHideHint) => "Shows when the pointer touches the bottom edge",
        (Lang::En, Msg::LiveGlass) => "Live glass",
        (Lang::En, Msg::LiveGlassHint) => "Shows what moves behind the dock as it happens",
        (Lang::En, Msg::OutlineSystem) => "Outline follows system colour",
        (Lang::En, Msg::OutlineSystemHint) => "The edge takes the accent colour of the desktop (window border)",
        (Lang::En, Msg::Magnification) => "Magnification",
        (Lang::En, Msg::MagnificationHint) => "Icons grow as the pointer passes over them",
        (Lang::En, Msg::Zoom) => "Zoom level",
        (Lang::En, Msg::SecRecommended) => "Recommended apps",
        (Lang::En, Msg::ShowRecommended) => "Show recommended apps",
        (Lang::En, Msg::ShowRecommendedHint) => {
            "Recently and frequently used apps that are closed and not kept in the dock"
        }
        (Lang::En, Msg::RecommendedCount) => "Number of recommended apps",
        (Lang::En, Msg::ClearHistory) => "Clear usage history",
        (Lang::En, Msg::HistoryCleared) => "History cleared",
        (Lang::Es, Msg::NewWindow) => "Nueva ventana",
        (Lang::Es, Msg::MinimizeOne) => "Minimizar",
        (Lang::Es, Msg::MinimizeAll) => "Minimizar todas",
        (Lang::Es, Msg::CloseOne) => "Cerrar",
        (Lang::Es, Msg::CloseAll) => "Cerrar todas las ventanas",
        (Lang::Es, Msg::MoveLeft) => "Mover a la izquierda",
        (Lang::Es, Msg::MoveRight) => "Mover a la derecha",
        (Lang::Es, Msg::Unpin) => "Quitar del dock",
        (Lang::Es, Msg::Pin) => "Fijar en el dock",
        (Lang::Es, Msg::AddPrompt) => "Añadir al dock",
        (Lang::Es, Msg::SettingsMenu) => "Ajustes del dock…",
        (Lang::Es, Msg::ForgetRecent) => "Quitar de recomendadas",
        (Lang::Es, Msg::SettingsTitle) => "Ajustes de GlassDock",
        (Lang::Es, Msg::SecAppearance) => "Apariencia",
        (Lang::Es, Msg::AutoHide) => "Ocultar automáticamente",
        (Lang::Es, Msg::AutoHideHint) => "Aparece al llevar el puntero al borde inferior",
        (Lang::Es, Msg::LiveGlass) => "Cristal en vivo",
        (Lang::Es, Msg::LiveGlassHint) => "Muestra en tiempo real lo que se mueve detrás",
        (Lang::Es, Msg::OutlineSystem) => "Contorno con el color del sistema",
        (Lang::Es, Msg::OutlineSystemHint) => "El borde toma el color de acento del escritorio (borde de ventana)",
        (Lang::Es, Msg::Magnification) => "Ampliación",
        (Lang::Es, Msg::MagnificationHint) => "Los iconos crecen al pasar el puntero",
        (Lang::Es, Msg::Zoom) => "Nivel de zoom",
        (Lang::Es, Msg::SecRecommended) => "Apps recomendadas",
        (Lang::Es, Msg::ShowRecommended) => "Mostrar apps recomendadas",
        (Lang::Es, Msg::ShowRecommendedHint) => {
            "Apps usadas reciente o frecuentemente que están cerradas y no fijadas en el dock"
        }
        (Lang::Es, Msg::RecommendedCount) => "Número de apps recomendadas",
        (Lang::Es, Msg::ClearHistory) => "Borrar historial de uso",
        (Lang::Es, Msg::HistoryCleared) => "Historial borrado",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Msg; 29] = [
        Msg::NewWindow,
        Msg::MinimizeOne,
        Msg::MinimizeAll,
        Msg::CloseOne,
        Msg::CloseAll,
        Msg::MoveLeft,
        Msg::MoveRight,
        Msg::Unpin,
        Msg::Pin,
        Msg::AddPrompt,
        Msg::SettingsMenu,
        Msg::ForgetRecent,
        Msg::SettingsTitle,
        Msg::SecAppearance,
        Msg::AutoHide,
        Msg::AutoHideHint,
        Msg::LiveGlass,
        Msg::LiveGlassHint,
        Msg::OutlineSystem,
        Msg::OutlineSystemHint,
        Msg::Magnification,
        Msg::MagnificationHint,
        Msg::Zoom,
        Msg::SecRecommended,
        Msg::ShowRecommended,
        Msg::ShowRecommendedHint,
        Msg::RecommendedCount,
        Msg::ClearHistory,
        Msg::HistoryCleared,
    ];

    #[test]
    fn spanish_locales_are_recognised() {
        assert_eq!(parse_lang("es_ES.UTF-8"), Lang::Es);
        assert_eq!(parse_lang("es"), Lang::Es);
        assert_eq!(parse_lang("ES_MX"), Lang::Es);
    }

    #[test]
    fn everything_else_is_english() {
        assert_eq!(parse_lang("en_US.UTF-8"), Lang::En);
        assert_eq!(parse_lang("C.UTF-8"), Lang::En);
        assert_eq!(parse_lang("de_DE"), Lang::En);
        assert_eq!(parse_lang(""), Lang::En);
    }

    #[test]
    fn every_message_is_translated_and_non_empty() {
        for m in ALL {
            assert!(!translate(Lang::En, m).is_empty());
            assert!(!translate(Lang::Es, m).is_empty());
            assert_ne!(
                translate(Lang::En, m),
                translate(Lang::Es, m),
                "{m:?} should differ between languages"
            );
        }
    }
}
