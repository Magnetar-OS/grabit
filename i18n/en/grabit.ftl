# Identity, consumed by build.rs to generate the desktop entry and metainfo.
app-title = grabit
app-comment = Selection-triggered actions for Wayland desktops
app-keywords = selection;clipboard;actions;popup;text;

# Shown when an action fails. Almost nothing else in the popup is translatable:
# every label and tooltip comes from a user's own action manifests.
action-failed = “{ $action }” failed

# The snippets flow: the selection itself is an action manifest.
install-action = Install this action
install-action-label = Install
install-title = grabit
install-done = Installed “{ $id }”.
install-done-disabled = Installed “{ $id }” disabled — review it, then enable it.
install-failed = Installing the action failed

# The result view, shown when an action has `after = "show"`.
result-copy = Copy result
result-close = Close

# The settings window (`grabit settings`).
settings-popup = Popup
settings-settle = Settle delay (ms)
settings-per-page = Actions per page
settings-icon-size = Icon size (px)
settings-offset-x = Horizontal offset (px)
settings-offset-y = Vertical offset (px)
settings-dismiss = Hide after the pointer leaves (ms, 0 disables)
settings-timeout = Hide unconditionally after (ms, 0 disables)
settings-animate = Entrance animation
settings-selection = Selection
settings-min-length = Minimum length
settings-max-length = Maximum length
settings-ignore-whitespace = Ignore whitespace-only selections
settings-apps = Excluded applications
settings-apps-hint = The bar never appears in these apps. App ids match exactly or by their last segment, so both “org.mozilla.firefox” and “firefox” work.
settings-exclude-placeholder = App id…
settings-exclude-add = Add
settings-remove = Remove
settings-actions = Actions
settings-actions-hint = Each action is one TOML file; the toggles and ordering here write those same files.
settings-move-up = Move up
settings-move-down = Move down
settings-edit-file = Edit the manifest
settings-open-folder = Open the actions folder
settings-disabled = disabled

# `grabit doctor`
doctor-capabilities = Wayland capabilities
doctor-selection = selection monitoring (data-control)
doctor-layer-shell = overlay placement (layer-shell)
doctor-virtual-keyboard = key injection (virtual-keyboard)
doctor-foreign-toplevel = per-app rules (foreign-toplevel)
doctor-shell-extension = GNOME Shell extension
doctor-frontend = front-end
doctor-frontend-layer = layer-shell
doctor-frontend-gnome = GNOME Shell extension
doctor-frontend-none = none — grabit cannot run here
doctor-no-paste = note: actions with `after = "replace"` will copy but not paste
doctor-config = config
doctor-actions = actions
doctor-actions-loaded = { $count } loaded
doctor-actions-failed = failed to load — { $error }
yes = yes
no = no
