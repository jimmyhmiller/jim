//! CEF helper process for jim's webview panes.
//!
//! CEF's multi-process architecture launches renderer/GPU/utility processes
//! from a separate executable. `make-bundle.sh` copies this binary into the
//! `jim Helper*.app` bundles inside `Jim.app`.
//!
//! It must stay dependency-light: it is copied five times into the bundle, and
//! it must not drag in jim-app or its libghostty dylib.

use cef::{args::Args, *};

fn main() {
    let args = Args::new();

    // Loads Chromium Embedded Framework.framework from the enclosing bundle.
    // `true` = helper process, which resolves the framework path differently
    // from the browser process.
    #[cfg(target_os = "macos")]
    let _loader = {
        let exe = std::env::current_exe().expect("current_exe");
        let loader = library_loader::LibraryLoader::new(&exe, true);
        assert!(
            loader.load(),
            "helper could not load Chromium Embedded Framework.framework"
        );
        loader
    };

    let _ = api_hash(sys::CEF_API_VERSION_LAST, 0);

    execute_process(
        Some(args.as_main_args()),
        None::<&mut App>,
        std::ptr::null_mut(),
    );
}
