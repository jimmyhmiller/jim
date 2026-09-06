;;; jim-integration-tests.el --- tests for jim integration -*- lexical-binding: t; -*-

(require 'ert)
(load (expand-file-name "jim-integration.el"
                        (file-name-directory (or load-file-name buffer-file-name)))
      nil t)

(ert-deftest jim-clamp-scroll-at-buffer-end-leaves-no-blank-page ()
  (with-temp-buffer
    (dotimes (line 100)
      (insert (format "line %d\n" (1+ line))))
    (let ((win (selected-window)))
      (set-window-buffer win (current-buffer))
      (goto-char (point-max))
      (forward-line -5)
      (set-window-start win (point))
      (jim--clamp-scroll-at-buffer-end win)
      (should (= (line-number-at-pos (window-start win))
                 (- 101 (window-body-height win))))
      (should (= (window-vscroll win t) 0)))))

(ert-deftest jim-clamp-scroll-at-buffer-end-keeps-short-buffer-at-top ()
  (with-temp-buffer
    (insert "one\ntwo\nthree\n")
    (let ((win (selected-window)))
      (set-window-buffer win (current-buffer))
      (jim--clamp-scroll-at-buffer-end win)
      (should (= (window-start win) (point-min)))
      (should (= (window-vscroll win t) 0)))))

(ert-deftest jim-native-display-rule-routes-coil-repl-below ()
  (let ((display-buffer-alist nil))
    (jim--setup-native-display-rules)
    (let ((rule (assoc "\\`\\*coil-repl: " display-buffer-alist)))
      (should rule)
      (should (memq 'jim--display-buffer-in-split-frame (nth 1 rule)))
      (should (equal (cdr (assq 'jim-split-dir (cddr rule))) 2)))))

(ert-deftest jim-display-buffer-in-split-frame-creates-sized-host-pane ()
  (let ((buffer (generate-new-buffer " *jim-display-test*"))
        params)
    (unwind-protect
        (cl-letf (((symbol-function 'make-frame)
                   (lambda (alist) (setq params alist) (selected-frame))))
          (let ((window (jim--display-buffer-in-split-frame
                         buffer '((jim-split-dir . 2)))))
            (should (eq (window-buffer window) buffer))
            (should (equal (cdr (assq 'window-system params)) 'jim))
            (should (equal (cdr (assq 'jim-split-dir params)) 2))))
      (kill-buffer buffer))))

(provide 'jim-integration-tests)

;;; jim-integration-tests.el ends here
