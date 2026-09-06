;;; jim-integration.el --- deep Jim <-> Emacs integration -*- lexical-binding: t; -*-

;; Loaded with `-l' by crates/jim-emacs/src/native.rs, which writes this
;; file to ~/.jim/emacs/ on every launch.  Everything here is runtime
;; lisp on purpose: `lisp/term/jim-win.el' in the emacs-jim fork is
;; preloaded (dumped), so changing it costs a re-dump, while this file
;; reloads on the next pane.
;;
;; It turns the port's raw draw-op transport into something that reads
;; as a GUI application:
;;
;;   * the whole jim design-token palette is applied to Emacs faces
;;     (`jim--apply-theme'), live, whenever the jim theme changes;
;;   * the trackpad scrolls by pixels through `pixel-scroll-precision'
;;     rather than by 2-line notches (`jim--scroll');
;;   * double/triple click select the word/line the port cannot report
;;     itself (its input records carry no timestamps, so Emacs never
;;     sees a multi-click);
;;   * Emacs reports where it is - buffer, file, mode, point, scroll
;;     extent - back to jim, which drives the pane title, the scroll
;;     indicator, and the file tree's selection.
;;
;; The control channel (jim--ctl-proc, opened by jim-win.el) is
;; newline-delimited text and is now duplex: jim -> emacs commands are
;; handled by `jim--ctl-dispatch' (redefined below), emacs -> jim events
;; go out through `jim--ctl-send'.

;;; Code:

(require 'cl-lib)
(require 'pixel-scroll)
(require 'json)
;; Optional: a build without tree-sitter still loads, `jim--setup-syntax'
;; just finds nothing to wire up.
(require 'treesit nil t)

(defgroup jim nil
  "Integration with the Jim editor's native Emacs pane."
  :group 'environment)

;; ---------------------------------------------------------------------
;; Frames <-> jim panes
;; ---------------------------------------------------------------------

(defun jim--frame (id)
  "The Emacs frame for jim pane/frame ID, or nil."
  (let ((f (gethash id jim--pane-frames)))
    (and (frame-live-p f) f)))

(defun jim--frame-id (frame)
  "The jim pane id for FRAME, or nil."
  (catch 'hit
    (maphash (lambda (id f) (when (eq f frame) (throw 'hit id)))
             jim--pane-frames)
    nil))

(defun jim--display-buffer-in-split-frame (buffer alist)
  "Display BUFFER in a native Jim pane and return its Emacs window.

Reuse an existing window for BUFFER.  Otherwise make a Jim frame whose
`jim-split-dir' parameter tells the host to dock it below the source pane.
ALIST may override the direction with a `jim-split-dir' entry (1 means
right, 2 means below)."
  (or (get-buffer-window buffer t)
      (let* ((dir (or (alist-get 'jim-split-dir alist) 2))
             (frame (make-frame `((window-system . jim)
                                  (jim-split-dir . ,dir))))
             (window (frame-selected-window frame)))
        (set-window-buffer window buffer)
        window)))

(defun jim--setup-native-display-rules ()
  "Route tool buffers that deserve a pane through Jim's native dock."
  ;; Coil owns creation and lifetime of its comint buffer.  This rule only
  ;; chooses where `coil-repl' / `pop-to-buffer' presents it, so the normal
  ;; Coil commands continue to work unchanged.
  (add-to-list 'display-buffer-alist
               '("\\`\\*coil-repl: "
                 (jim--display-buffer-in-split-frame)
                 (jim-split-dir . 2))))

;; ---------------------------------------------------------------------
;; emacs -> jim
;; ---------------------------------------------------------------------

(defun jim--ctl-send (line)
  "Send LINE (no newline) to jim over the control channel."
  (when (process-live-p jim--ctl-proc)
    (ignore-errors
      (process-send-string jim--ctl-proc (concat line "\n")))))

;; ---------------------------------------------------------------------
;; Theme: jim design tokens -> Emacs faces
;; ---------------------------------------------------------------------

(defcustom jim-adopt-theme t
  "Whether jim's design tokens drive Emacs's faces.

Non-nil (the default) means an Emacs pane repaints itself to match
whatever theme jim is wearing, live.  Set it to nil in your init file to
keep your own Emacs theme instead; jim will still send its palette, and
`jim--theme' will still hold it, but no face is touched."
  :type 'boolean :group 'jim)

(defvar jim--theme nil
  "The most recent jim palette, an alist of token name (string) -> \"#rrggbb\".")

(defun jim--tok (name &optional fallback)
  "Color for jim token NAME, else FALLBACK (else nil)."
  (or (cdr (assoc name jim--theme)) fallback))

(defun jim--luma (hex)
  "Rough perceptual luminance of HEX (\"#rrggbb\"), 0.0-1.0."
  (if (and (stringp hex) (= (length hex) 7))
      (let ((r (/ (string-to-number (substring hex 1 3) 16) 255.0))
            (g (/ (string-to-number (substring hex 3 5) 16) 255.0))
            (b (/ (string-to-number (substring hex 5 7) 16) 255.0)))
        (+ (* 0.2126 r) (* 0.7152 g) (* 0.0722 b)))
    0.5))

(defun jim--mix (a b f)
  "Blend hex colors A and B, F of the way from A to B."
  (if (and (stringp a) (stringp b) (= (length a) 7) (= (length b) 7))
      (apply #'format "#%02x%02x%02x"
             (cl-loop for i from 1 to 5 by 2
                      collect (let ((x (string-to-number (substring a i (+ i 2)) 16))
                                    (y (string-to-number (substring b i (+ i 2)) 16)))
                                (max 0 (min 255 (round (+ x (* f (- y x)))))))))
    (or a b)))

;; The port cannot render weight or slant: `draw_glyph_string' in
;; jimwin.coil emits every run with font id 0 and no attribute flags, so
;; a :weight/:slant/:underline attribute changes nothing on screen (and
;; the font driver has no bold face of Andale Mono to open anyway).
;; Everything below therefore carries meaning in COLOR alone - which is
;; also what jim's own syntax tokens are built around.
(defun jim--faces ()
  "Face specs derived from the current jim palette.
Each element is (FACE . ATTRIBUTE-PLIST)."
  (let* ((bg      (jim--tok "bg" "#101418"))
         (fg      (jim--tok "fg" "#e6e6e6"))
         (muted   (jim--tok "fg_muted" (jim--mix fg bg 0.45)))
         (accent  (jim--tok "accent" "#6fb3e0"))
         (caret   (jim--tok "caret" accent))
         (sel     (jim--tok "selection" (jim--mix bg accent 0.30)))
         (warn    (jim--tok "warn" "#c9a96a"))
         (err     (jim--tok "err" "#e06c75"))
         (panel   (jim--tok "pane_bg" (jim--mix bg fg 0.06)))
         (chrome  (jim--tok "chrome_title_bg" panel))
         (chromef (jim--tok "chrome_title" muted))
         (chromea (jim--tok "chrome_title_focused" fg))
         (divider (jim--tok "chrome_divider" (jim--mix bg fg 0.18)))
         (dark    (< (jim--luma bg) 0.5))
         ;; A "just off the background" wash used for hl-line, the
         ;; fringe, inactive chrome - anything that should read as a
         ;; surface rather than as content.
         (subtle  (jim--mix bg fg 0.07))
         (syn (lambda (name fallback) (jim--tok (concat "syntax_" name) fallback))))
    `((default              :background ,bg :foreground ,fg)
      (cursor               :background ,caret :foreground ,bg)
      (region               :background ,sel :foreground unspecified
                            :extend t)
      (secondary-selection  :background ,(jim--mix bg sel 0.6))
      (highlight            :background ,(jim--mix bg accent 0.22) :foreground unspecified)
      (hl-line              :background ,subtle :extend t)
      (fringe               :background ,bg :foreground ,muted)
      (shadow               :foreground ,muted)
      (link                 :foreground ,accent :underline t)
      (link-visited         :foreground ,(jim--mix accent fg 0.35) :underline t)
      (minibuffer-prompt    :foreground ,accent)
      (escape-glyph         :foreground ,warn)
      (homoglyph            :foreground ,warn)
      (error                :foreground ,err)
      (warning              :foreground ,warn)
      (success              :foreground ,(funcall syn "string" accent))
      (trailing-whitespace  :background ,(jim--mix bg err 0.4))
      (whitespace-trailing  :background ,(jim--mix bg err 0.4) :foreground unspecified)

      ;; Chrome.  jim already draws a pane title bar with the buffer
      ;; name and mode, so the mode line's job here is the *rest* of the
      ;; status - it reads as a quiet footer, not a second title bar.
      (mode-line            :background ,chrome :foreground ,chromea
                            :box nil :overline nil :underline nil)
      (mode-line-active     :background ,chrome :foreground ,chromea :box nil)
      (mode-line-inactive   :background ,(jim--mix bg chrome 0.5) :foreground ,chromef
                            :box nil :overline nil :underline nil)
      (mode-line-highlight  :background ,(jim--mix chrome accent 0.35) :box nil)
      (mode-line-buffer-id  :foreground ,accent)
      (mode-line-emphasis   :foreground ,accent)
      (header-line          :background ,panel :foreground ,fg :box nil)
      (tab-bar              :background ,panel :foreground ,muted)
      (tab-bar-tab          :background ,bg :foreground ,fg)
      (tab-bar-tab-inactive :background ,panel :foreground ,muted)
      (vertical-border      :foreground ,divider :background ,divider)
      (window-divider       :foreground ,divider)
      (window-divider-first-pixel :foreground ,divider)
      (window-divider-last-pixel  :foreground ,divider)
      (internal-border      :background ,bg)
      (line-number          :background ,bg :foreground ,(jim--mix bg fg 0.35))
      (line-number-current-line :background ,bg :foreground ,accent)
      (fill-column-indicator :foreground ,(jim--mix bg fg 0.15))

      ;; Search / matching.
      (isearch              :background ,accent
                            :foreground ,(if dark bg fg))
      (isearch-fail         :background ,(jim--mix bg err 0.5) :foreground ,fg)
      (lazy-highlight       :background ,(jim--mix bg accent 0.30) :foreground unspecified)
      (match                :background ,(jim--mix bg warn 0.35))
      (show-paren-match     :background ,(jim--mix bg accent 0.45) :foreground ,fg)
      (show-paren-mismatch  :background ,err :foreground ,bg)

      ;; Completion UIs (harmless if the face is not defined).
      (completions-common-part      :foreground ,accent)
      (completions-first-difference :foreground ,warn)
      (completions-annotations      :foreground ,muted)

      ;; Syntax - straight from jim's own syntax tokens, so an Emacs
      ;; buffer and a jim editor pane colour the same code the same way.
      (font-lock-comment-face            :foreground ,(funcall syn "comment" muted))
      (font-lock-comment-delimiter-face  :foreground ,(funcall syn "comment" muted))
      (font-lock-doc-face                :foreground ,(funcall syn "comment" muted))
      (font-lock-doc-markup-face         :foreground ,(funcall syn "attribute" accent))
      (font-lock-string-face             :foreground ,(funcall syn "string" accent))
      (font-lock-keyword-face            :foreground ,(funcall syn "keyword" accent))
      (font-lock-builtin-face            :foreground ,(funcall syn "keyword" accent))
      (font-lock-preprocessor-face       :foreground ,(funcall syn "attribute" accent))
      (font-lock-function-name-face      :foreground ,(funcall syn "function" fg))
      (font-lock-function-call-face      :foreground ,(funcall syn "function" fg))
      (font-lock-variable-name-face      :foreground ,(funcall syn "variable" fg))
      (font-lock-variable-use-face       :foreground ,(funcall syn "variable" fg))
      (font-lock-property-name-face      :foreground ,(funcall syn "property" fg))
      (font-lock-property-use-face       :foreground ,(funcall syn "property" fg))
      (font-lock-type-face               :foreground ,(funcall syn "type" fg))
      (font-lock-constant-face           :foreground ,(funcall syn "constant" fg))
      (font-lock-number-face             :foreground ,(funcall syn "constant" fg))
      (font-lock-operator-face           :foreground ,(funcall syn "operator" fg))
      (font-lock-punctuation-face        :foreground ,(funcall syn "punctuation" muted))
      (font-lock-delimiter-face          :foreground ,(funcall syn "punctuation" muted))
      (font-lock-bracket-face            :foreground ,(funcall syn "punctuation" muted))
      (font-lock-escape-face             :foreground ,(funcall syn "escape" warn))
      (font-lock-regexp-face             :foreground ,(funcall syn "escape" warn))
      (font-lock-negation-char-face      :foreground ,(funcall syn "operator" warn))
      (font-lock-warning-face            :foreground ,warn)
      (font-lock-misc-punctuation-face   :foreground ,(funcall syn "punctuation" muted))
      (font-lock-constant-face           :foreground ,(funcall syn "constant" fg))
      (font-lock-label-face              :foreground ,(funcall syn "label" accent))
      (font-lock-preprocessor-face       :foreground ,(funcall syn "attribute" accent))

      ;; Dired / diff / vc, so the "other" buffers do not fall back to a
      ;; default theme that assumes a light background.
      (diff-added           :foreground ,(funcall syn "string" accent) :background unspecified)
      (diff-removed         :foreground ,err :background unspecified)
      (diff-header          :background ,panel :foreground ,fg)
      (diff-file-header     :background ,panel :foreground ,accent))))

(defun jim--set-face (face attrs)
  "Apply ATTRS to FACE on every frame, ignoring unknown faces."
  (when (facep face)
    (ignore-errors
      (apply #'set-face-attribute face nil attrs))))

(defun jim--apply-theme (palette)
  "Apply PALETTE, an alist of jim token name -> \"#rrggbb\", to Emacs faces.
Does nothing but remember PALETTE when `jim-adopt-theme' is nil."
  (setq jim--theme palette)
  (when jim-adopt-theme
    (jim--apply-theme-1)))

(defun jim--apply-theme-1 ()
  "Repaint every face from `jim--theme'."
  (let* ((specs (jim--faces))
         (bg (jim--tok "bg" "#101418"))
         (fg (jim--tok "fg" "#e6e6e6"))
         (caret (jim--tok "caret" (jim--tok "accent" fg))))
    ;; The frame's own colours first: the port paints cleared regions
    ;; with FRAME_BACKGROUND_PIXEL, which follows the `default' face.
    (pcase-dolist (`(,face . ,attrs) specs)
      (jim--set-face face attrs))
    (dolist (f (frame-list))
      (ignore-errors
        (modify-frame-parameters
         f `((background-color . ,bg)
             (foreground-color . ,fg)
             (cursor-color . ,caret)))))
    (setq default-frame-alist
          (cons (cons 'background-color bg)
                (cons (cons 'foreground-color fg)
                      (cons (cons 'cursor-color caret)
                            (cl-remove-if (lambda (c)
                                            (memq (car-safe c)
                                                  '(background-color
                                                    foreground-color
                                                    cursor-color)))
                                          default-frame-alist)))))
    ;; A theme loaded by the user's init may have baked its own colours
    ;; into faces we did not touch; repaint everything either way.
    (force-mode-line-update t)
    (redraw-display)))

;; ---------------------------------------------------------------------
;; Font
;; ---------------------------------------------------------------------

(defun jim--set-font-pixels (px)
  "Set the default font to PX pixels tall across every jim frame.

jim sizes its own UI in pixels (the `font_size' design token), and the
port reports 96dpi, so asking Emacs for a point size lands a third too
big and only lines up by luck.  A fontconfig `:pixelsize=' spec is exact:
PX in means PX out, and Emacs text matches the rest of the app.

The family is left alone - whatever the user's init chose stays."
  (when (and (integerp px) (> px 0))
    (let ((spec nil))
      (dolist (f (frame-list))
        (let ((fam (face-attribute 'default :family f)))
          (when (or (null fam) (eq fam 'unspecified))
            (setq fam "Menlo"))
          (setq spec (format "%s:pixelsize=%d" fam px))
          ;; KEEP-SIZE t: change the font, keep the pane's pixel size.
          (ignore-errors (set-frame-font spec t (list f)))))
      ;; Frames created later (new panes) start at the same size.
      (when spec
        (setq default-frame-alist
              (cons (cons 'font spec)
                    (assq-delete-all 'font default-frame-alist)))))))

;; ---------------------------------------------------------------------
;; Scrolling
;; ---------------------------------------------------------------------

(defcustom jim-cursor-type '(bar . 2)
  "`cursor-type' for jim panes.

A GUI editor has a caret, not a terminal's filled cell.  The port draws
bar and hbar cursors as a thin rect that jim fills with its own `caret'
design token, so the caret tracks the theme live; `box' and `hollow' fall
back to the inverted-glyph block.  Set to `box' for the old look."
  :type 'sexp :group 'jim)

(defcustom jim-line-number-limit 400000
  "Buffer size above which jim stops reporting the current line number."
  :type 'integer :group 'jim)

(defcustom jim-scroll-pixel-scale 1.0
  "Multiplier applied to the pixel deltas jim reports from the trackpad."
  :type 'number :group 'jim)

(defun jim--window-at (fid x y)
  "The window at frame-pixel X,Y in jim pane FID.
Falls back to that frame's selected window."
  (let ((f (jim--frame fid)))
    (when f
      (or (ignore-errors
            (let ((w (posn-window (posn-at-x-y x y f t))))
              (and (windowp w) (eq (window-frame w) f) w)))
          (frame-selected-window f)))))

(defun jim--clamp-scroll-at-buffer-end (win)
  "Keep the last non-empty line at the bottom of WIN.

Emacs normally allows scrolling until the last line reaches the top of
the window.  That is useful for editing, but leaves an almost entirely
blank jim pane.  Once the end of the buffer is visible, align its last
non-empty line with the bottom instead.  `recenter' handles wrapped and
variable-height lines using redisplay's own measurements."
  (when (and (window-live-p win)
             (>= (window-end win t)
                 (with-current-buffer (window-buffer win) (point-max))))
    (with-selected-window win
      (save-excursion
        (goto-char (point-max))
        ;; A final newline puts point on a synthetic empty line.  The
        ;; preceding character belongs to the last line users perceive
        ;; as buffer content.
        (when (and (> (point) (point-min)) (bolp))
          (backward-char 1))
        (recenter -1)))))

(defun jim--scroll (fid x y dy)
  "Scroll the window at X,Y in pane FID by DY pixels.
Positive DY scrolls toward the beginning of the buffer, matching a
wheel-up / two-finger-down gesture."
  (let ((win (jim--window-at fid x y)))
    (when (window-live-p win)
      (let ((delta (round (* dy jim-scroll-pixel-scale))))
        (unless (zerop delta)
          (with-selected-window win
            (condition-case nil
                (if (> (abs delta) (window-text-height win t))
                    ;; A fling larger than the window cannot be done as a
                    ;; vscroll; fall back to whole lines.
                    (let ((lines (/ delta (max 1 (default-line-height)))))
                      (if (> lines 0) (scroll-down lines) (scroll-up (- lines))))
                  (if (> delta 0)
                      (pixel-scroll-precision-scroll-up delta)
                    (pixel-scroll-precision-scroll-down (- delta))))
              (beginning-of-buffer nil)
              (end-of-buffer nil)
              (error nil))
            ;; Clamp after both normal precision scrolling and the
            ;; whole-line fallback used for large trackpad flings.
            (when (< delta 0)
              (jim--clamp-scroll-at-buffer-end win)))))
      ;; Immediately, so the pane's scroll indicator tracks the gesture
      ;; without waiting for idle; then again once redisplay has run and
      ;; `window-end' is trustworthy.
      (jim--report-state t fid)
      (jim--report-state-soon fid))))

;; ---------------------------------------------------------------------
;; Multi-click selection
;; ---------------------------------------------------------------------
;;
;; The port's 24-byte input record has no timestamp field, so
;; `make_lispy_event' can never pair two clicks into a double-click
;; (keyboard.c requires a non-zero `button_down_time').  jim detects the
;; multi-click itself and asks for the selection here.

(defun jim--click (fid x y count)
  "Select the word (COUNT 2) or line (COUNT >= 3) at X,Y in pane FID."
  (let ((win (jim--window-at fid x y)))
    (when (window-live-p win)
      (with-selected-window win
        (let ((pos (posn-point (posn-at-x-y x y (window-frame win) t))))
          (when (integerp pos)
            (goto-char pos)
            (condition-case nil
                (if (>= count 3)
                    (progn (beginning-of-line)
                           (push-mark (line-end-position) t t))
                  (let ((bounds (bounds-of-thing-at-point 'word)))
                    (when bounds
                      (goto-char (car bounds))
                      (push-mark (cdr bounds) t t))))
              (error nil)))))
      (jim--report-state t fid))))

;; ---------------------------------------------------------------------
;; State: emacs -> jim
;; ---------------------------------------------------------------------

(defvar jim--last-state (make-hash-table :test 'eql)
  "Last state payload sent per pane id, so we only send on change.")

(defun jim--state-of (fid)
  "The state plist jim cares about for pane FID, or nil."
  (let ((f (jim--frame fid)))
    (when f
      (let* ((win (frame-selected-window f))
             (buf (window-buffer win)))
        (with-current-buffer buf
          (let* ((size (max 1 (- (point-max) (point-min))))
                 (start (window-start win))
                 (end (or (ignore-errors (window-end win t)) (point-max))))
            (list :pane fid
                  :buffer (buffer-name buf)
                  :path (and buffer-file-name (expand-file-name buffer-file-name))
                  :dir (expand-file-name default-directory)
                  :modified (if (buffer-modified-p) t :json-false)
                  :readonly (if buffer-read-only t :json-false)
                  :mode (format-mode-line mode-name nil nil buf)
                  ;; `line-number-at-pos' counts from point-min, and this
                  ;; runs after every command; do not pay that on a huge
                  ;; buffer just to label a status line.
                  :line (if (< size jim-line-number-limit)
                            (line-number-at-pos (window-point win) t)
                          0)
                  :column (save-excursion
                            (goto-char (window-point win))
                            (current-column))
                  ;; Fractions of the buffer above / below the viewport.
                  ;; Character-based on purpose: counting lines is O(n)
                  ;; and this runs after every command.
                  :top (/ (float (- start (point-min))) size)
                  :bottom (min 1.0 (/ (float (- end (point-min))) size)))))))))

(defun jim--report-state (&optional force fid)
  "Send pane FID's state to jim when it has changed (or FORCE).
FID defaults to the selected frame's pane."
  (let ((fid (or fid (jim--frame-id (selected-frame)))))
    (when fid
      (let* ((state (jim--state-of fid))
             (json (and state (json-encode state))))
        (when (and json (or force (not (equal json (gethash fid jim--last-state)))))
          (puthash fid json jim--last-state)
          (jim--ctl-send (format "state %d %s" fid json)))))))

(defvar jim--state-timer nil
  "Pending idle timer for `jim--report-state-soon'.")

(defun jim--report-state-soon (&optional fid &rest _)
  "Report pane FID's state (default: the selected frame) once display settles.

`window-end' is only correct after redisplay, so a report fired straight
out of `post-command-hook' can carry a stale viewport bottom (visibly:
the scroll indicator sized as if the whole buffer fit).  An idle timer
runs after redisplay, and the change-detection in `jim--report-state'
means a report that was already right costs nothing."
  ;; `window-scroll-functions' and friends call their hooks with their
  ;; own arguments; only an explicit integer here is a pane id.
  (unless (integerp fid)
    (setq fid nil))
  (when (timerp jim--state-timer)
    (cancel-timer jim--state-timer))
  (setq jim--state-timer
        (run-with-idle-timer 0 nil
                             (lambda ()
                               (setq jim--state-timer nil)
                               (jim--report-state nil fid)))))

;; ---------------------------------------------------------------------
;; jim -> emacs commands
;; ---------------------------------------------------------------------

(defun jim--focus (fid)
  "Make pane FID's frame the selected one.
jim owns focus - it decides which pane the keyboard is talking to - so
Emacs's idea of the selected frame has to follow it, or `M-x' and every
state report would land on whichever frame Emacs happened to select
last."
  (let ((f (jim--frame fid)))
    (when (and f (not (eq f (selected-frame))))
      (ignore-errors (select-frame f))
      (jim--report-state t fid))))

(defun jim--run-command (fid name)
  "Run interactive command NAME in pane FID's selected window."
  (let ((f (jim--frame fid))
        (sym (intern-soft name)))
    (when (and f (commandp sym))
      (with-selected-frame f
        (with-selected-window (frame-selected-window f)
          (condition-case err
              (call-interactively sym)
            (error (message "jim: %s: %s" name (error-message-string err)))))))))

(defun jim--coalesce-scrolls (lines)
  "Merge each run of consecutive same-frame `scroll' commands in LINES.

Scrolling up is about five times dearer than scrolling down -
`pixel-scroll-precision-scroll-up' has to lay text out backwards from
`window-start', where scrolling down just walks forward from a position
it already has - so a fast upward flick can hand Emacs commands quicker
than it retires them.  Left alone they queue in the socket and the view
keeps gliding after your fingers have stopped.

Merging needs no timer and no backpressure, and cannot stall: when Emacs
is keeping up each filter call holds one line and nothing changes; when
it falls behind, the surplus lines are already sitting in the same chunk
and collapse into one larger scroll - which is also cheaper than the sum
of its parts, since 300px in one call costs one backwards layout instead
of ten.  Any non-scroll line ends the run, so ordering against
`open' / `cmd' / `theme' is preserved."
  (let ((re (concat "\\`scroll \\([0-9]+\\) \\(-?[0-9]+\\)"
                    " \\(-?[0-9]+\\) \\(-?[0-9]+\\)\\'"))
        out)
    (dolist (line lines (nreverse out))
      (if (not (string-match re line))
          (push line out)
        (let* ((fid (match-string 1 line))
               (x (match-string 2 line))
               (y (match-string 3 line))
               (dy (string-to-number (match-string 4 line)))
               (prev (car out))
               (folded (and prev
                            (string-match re prev)
                            (equal (match-string 1 prev) fid)
                            (+ dy (string-to-number (match-string 4 prev))))))
          ;; Same frame and still consecutive: fold into the previous
          ;; command, keeping the newest pointer position.
          (if folded
              (setcar out (format "scroll %s %s %s %d" fid x y folded))
            (push (format "scroll %s %s %s %d" fid x y dy) out)))))))

;; Overrides the one in jim-win.el (which is preloaded, so it cannot
;; batch): `jim--ctl-connect' runs from `window-setup-hook', after this
;; file has loaded, so it installs THIS definition as the filter.
(defun jim--ctl-filter (_proc string)
  "Process filter: split STRING into control lines, coalescing scrolls."
  (setq jim--ctl-buffer (concat jim--ctl-buffer string))
  (let (lines)
    (while (string-match "\n" jim--ctl-buffer)
      (push (substring jim--ctl-buffer 0 (match-beginning 0)) lines)
      (setq jim--ctl-buffer (substring jim--ctl-buffer (match-end 0))))
    (dolist (line (jim--coalesce-scrolls (nreverse lines)))
      (jim--ctl-dispatch line))))

(defun jim--ctl-dispatch (line)
  "Handle one control-channel LINE from jim.

Replaces the minimal dispatcher in jim-win.el.  Commands:

  open  <fid> <path>            find-file PATH in that pane
  font  <pts>                   default font size in points, all panes
  font-px <px>                  default font size in PIXELS, all panes
  theme <json>                  design-token palette -> faces
  scroll <fid> <x> <y> <dy>     pixel-precision scroll at a frame pixel
  click <fid> <x> <y> <count>   double/triple-click selection
  cmd   <fid> <command>         run an interactive command
  eval  <sexp>                  escape hatch
  state <fid>                   force a state report"
  (cond
   ((string-match "\\`open \\([0-9]+\\) \\(.*\\)\\'" line)
    (let ((id (string-to-number (match-string 1 line)))
          (path (match-string 2 line)))
      (ignore-errors (jim--find-file-in-frame id path))
      (jim--report-state t)))
   ((string-match "\\`font \\([0-9]+\\)\\'" line)
    (ignore-errors (jim--set-font-size (string-to-number (match-string 1 line)))))
   ((string-match "\\`font-px \\([0-9]+\\)\\'" line)
    (ignore-errors (jim--set-font-pixels (string-to-number (match-string 1 line)))))
   ((string-match "\\`theme \\(.*\\)\\'" line)
    (let ((palette (ignore-errors
                     (let ((json-object-type 'alist)
                           (json-key-type 'string))
                       (json-read-from-string (match-string 1 line))))))
      (when palette (jim--apply-theme palette))))
   ((string-match "\\`scroll \\([0-9]+\\) \\(-?[0-9]+\\) \\(-?[0-9]+\\) \\(-?[0-9]+\\)\\'" line)
    (jim--scroll (string-to-number (match-string 1 line))
                 (string-to-number (match-string 2 line))
                 (string-to-number (match-string 3 line))
                 (string-to-number (match-string 4 line))))
   ((string-match "\\`click \\([0-9]+\\) \\(-?[0-9]+\\) \\(-?[0-9]+\\) \\([0-9]+\\)\\'" line)
    (jim--click (string-to-number (match-string 1 line))
                (string-to-number (match-string 2 line))
                (string-to-number (match-string 3 line))
                (string-to-number (match-string 4 line))))
   ((string-match "\\`cmd \\([0-9]+\\) \\(.*\\)\\'" line)
    (jim--run-command (string-to-number (match-string 1 line))
                      (match-string 2 line))
    (jim--report-state t))
   ((string-match "\\`eval \\(.*\\)\\'" line)
    (condition-case err
        (eval (car (read-from-string (match-string 1 line))) t)
      (error (message "jim eval: %s" (error-message-string err)))))
   ((string-match "\\`focus \\([0-9]+\\)\\'" line)
    (jim--focus (string-to-number (match-string 1 line))))
   ((string-match "\\`state \\([0-9]+\\)\\'" line)
    (jim--report-state t (string-to-number (match-string 1 line))))))

;; ---------------------------------------------------------------------
;; GUI behaviour
;; ---------------------------------------------------------------------

;; ---------------------------------------------------------------------
;; Syntax highlighting
;; ---------------------------------------------------------------------

(defcustom jim-treesit-font-lock-level 4
  "`treesit-font-lock-level' to use in jim panes.

Level 4 fontifies variables, properties, operators and brackets on top of
the level-3 default - which is the whole point here, because jim's theme
has a design token for every one of those and level 3 leaves them all the
plain foreground colour."
  :type 'integer :group 'jim)

(defconst jim--treesit-modes
  '((rust       "\\.rs\\'"                 rust-ts-mode)
    (toml       "\\.toml\\'"               toml-ts-mode      conf-toml-mode)
    (json       "\\.json\\'"               json-ts-mode      js-json-mode)
    (yaml       "\\.ya?ml\\'"              yaml-ts-mode)
    (go         "\\.go\\'"                 go-ts-mode)
    (python     "\\.py[iw]?\\'"            python-ts-mode    python-mode)
    (typescript "\\.ts\\'"                 typescript-ts-mode)
    (tsx        "\\.tsx\\'"                tsx-ts-mode)
    (c          "\\.c\\'"                  c-ts-mode         c-mode)
    (cpp        "\\.\\(cc\\|cpp\\|hpp\\)\\'" c++-ts-mode  c++-mode)
    (bash       "\\.\\(sh\\|bash\\)\\'"       bash-ts-mode sh-mode))
  "(LANGUAGE FILE-REGEXP TS-MODE [CLASSIC-MODE]) for tree-sitter majors.

Only entries whose grammar is actually installed are wired up, so an
absent grammar changes nothing rather than erroring.  Install one with
\\[treesit-install-language-grammar]; it takes effect on the next pane.")

(defun jim--setup-syntax ()
  "Route files to their tree-sitter major mode, where a grammar exists.

Emacs 30 ships `rust-ts-mode' and friends but wires none of them into
`auto-mode-alist', so a .rs file lands in Fundamental mode with no colour
at all unless something opts in.  This opts in, per language, only when
the grammar is present."
  (when (and (fboundp 'treesit-available-p) (treesit-available-p)
             (boundp 'treesit-font-lock-level))
    (setq treesit-font-lock-level jim-treesit-font-lock-level)
    (pcase-dolist (`(,lang ,regexp ,ts-mode ,classic) jim--treesit-modes)
      (when (and (treesit-language-available-p lang) (fboundp ts-mode))
        (add-to-list 'auto-mode-alist (cons regexp ts-mode))
        ;; A buffer routed to the classic mode by something else follows
        ;; too, rather than being the one file that stays uncoloured.
        (when (and classic (boundp 'major-mode-remap-alist))
          (add-to-list 'major-mode-remap-alist (cons classic ts-mode)))))
    ;; Buffers already visited before this ran (the *scratch* pane's
    ;; first file, say) do not re-pick their mode on their own.
    (dolist (buf (buffer-list))
      (with-current-buffer buf
        (when (and buffer-file-name (eq major-mode 'fundamental-mode))
          (ignore-errors (set-auto-mode)))))))

(defun jim--setup ()
  "Bring the Emacs side up once jim's control channel exists."
  ;; Pixel-precision scrolling: we drive `pixel-scroll-precision-scroll-*'
  ;; directly from jim's trackpad deltas, so the minor mode's own
  ;; wheel bindings are not needed - but its interpolation state and
  ;; `window-vscroll' handling are.
  (setq pixel-scroll-precision-interpolate-page nil
        pixel-scroll-precision-use-momentum nil
        ;; A vscroll'd window must not be re-centred behind our back.
        auto-window-vscroll nil
        scroll-conservatively 101
        scroll-margin 0
        scroll-step 0
        ;; jim's pane title bar already carries buffer + mode, so the
        ;; frame does not need to fight for the same information.
        ring-bell-function #'ignore
        use-dialog-box nil
        use-file-dialog nil
        inhibit-startup-screen t)
  (setq-default cursor-type jim-cursor-type)
  ;; A bar caret is easy to lose track of when it is not blinking, and
  ;; unlike a block it does not obscure the glyph it sits on.
  (blink-cursor-mode 1)
  (pixel-scroll-precision-mode 1)
  ;; No thick window dividers. jim-win.el turns them on for "GUI-style
  ;; splits", but in jim a split IS a pane - `C-x 2' / `C-x 3' are bound
  ;; to `jim-split-window-below' / `-right', which make a new frame that
  ;; jim docks beside this one - so an in-frame divider almost never has
  ;; anything to divide. It also does not survive Emacs's scroll
  ;; optimization: the port draws the 8px bottom divider once, a later
  ;; `scroll_run' blits it up into the middle of the buffer, and nothing
  ;; ever repaints that strip - a solid `chrome_divider'-coloured band
  ;; straight through a line of code. Side-by-side windows still get the
  ;; classic 1px `vertical-border', which redraws normally.
  (window-divider-mode -1)
  (setq window-divider-default-places nil
        window-divider-default-right-width 1
        window-divider-default-bottom-width 1)
  (jim--setup-native-display-rules)
  (jim--setup-syntax)
  (add-hook 'post-command-hook #'jim--report-state-soon)
  (add-hook 'window-configuration-change-hook #'jim--report-state-soon)
  (add-hook 'window-scroll-functions #'jim--report-state-soon)
  (add-hook 'buffer-list-update-hook #'jim--report-state-soon)
  (jim--report-state t))

;; jim-win.el opens the control channel from `window-setup-hook'; append
;; so we run after it and can talk back immediately.
(add-hook 'window-setup-hook #'jim--setup t)

(provide 'jim-integration)

;;; jim-integration.el ends here
