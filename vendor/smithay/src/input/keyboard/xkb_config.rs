pub use xkbcommon::xkb;

/// Configuration for xkbcommon.
///
/// For the fields that are not set ("" or None, as set in the `Default` impl), xkbcommon will use
/// the values from the environment variables `XKB_DEFAULT_RULES`, `XKB_DEFAULT_MODEL`,
/// `XKB_DEFAULT_LAYOUT`, `XKB_DEFAULT_VARIANT` and `XKB_DEFAULT_OPTIONS`.
///
/// For details, see the [documentation at xkbcommon.org][docs].
///
/// [docs]: https://xkbcommon.org/doc/current/structxkb__rule__names.html
#[derive(Clone, Debug, Default)]
pub struct XkbConfig<'a> {
    /// The rules file to use.
    ///
    /// The rules file describes how to interpret the values of the model, layout, variant and
    /// options fields.
    pub rules: &'a str,
    /// The keyboard model by which to interpret keycodes and LEDs.
    pub model: &'a str,
    /// A comma separated list of layouts (languages) to include in the keymap.
    pub layout: &'a str,
    /// A comma separated list of variants, one per layout, which may modify or augment the
    /// respective layout in various ways.
    pub variant: &'a str,
    /// A comma separated list of options, through which the user specifies non-layout related
    /// preferences, like which key combinations are used for switching layouts, or which key is the
    /// Compose key.
    pub options: Option<String>,
    /// An already-compiled keymap in `XKB_KEYMAP_FORMAT_TEXT_V1`, used **instead** of
    /// resolving the RMLVO fields above.
    ///
    /// WAYCOMPOSE FORK. Upstream has no way to build a keyboard from a keymap string:
    /// [`KeyboardHandle::set_keymap_from_string`] exists but only after
    /// [`Seat::add_keyboard`] has already succeeded, and `add_keyboard` goes through
    /// `xkb_keymap_new_from_names`. On Windows that call **cannot** succeed and must not,
    /// because it resolves RMLVO names against the xkeyboard-config data tree — ~2,000 files
    /// of X11 configuration that does not exist there, and which this project deliberately
    /// does not ship (docs/design.md §7.2, and `nix/libxkbcommon-mingw.nix` compiles in a
    /// config root nobody creates so that a `new_from_names` which silently started working
    /// would be a visible bug).
    ///
    /// So the split docs/design.md §7.2 describes needs one hook to hang on: RMLVO names are
    /// resolved on Linux at build time (`scripts/gen-keymap.sh`) and the Windows side only
    /// ever compiles a *string*. This field is that hook, and §7.2 predicted it exactly —
    /// *"an `XkbConfig` variant carrying a keymap string, which is small and probably
    /// upstreamable"*.
    ///
    /// `None` on every platform that has the data tree, so nothing about the Unix behaviour
    /// changes and the field is invisible to callers using `..XkbConfig::default()`.
    ///
    /// [`KeyboardHandle::set_keymap_from_string`]: super::KeyboardHandle::set_keymap_from_string
    /// [`Seat::add_keyboard`]: crate::input::Seat::add_keyboard
    pub keymap: Option<&'a str>,
}

impl XkbConfig<'_> {
    pub(crate) fn compile_keymap(&self, context: &xkb::Context) -> Result<xkb::Keymap, ()> {
        // WAYCOMPOSE FORK: a supplied keymap short-circuits the RMLVO lookup. Checked first
        // rather than as a fallback, because a fallback would mean the failure mode on
        // Windows is "the include path error is logged and then the keymap works", which
        // reads as a warning about something harmless and is how a broken data tree stays
        // broken.
        if let Some(keymap) = self.keymap {
            return xkb::Keymap::new_from_string(
                context,
                keymap.to_owned(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .ok_or(());
        }
        xkb::Keymap::new_from_names(
            context,
            self.rules,
            self.model,
            self.layout,
            self.variant,
            self.options.clone(),
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .ok_or(())
    }
}
