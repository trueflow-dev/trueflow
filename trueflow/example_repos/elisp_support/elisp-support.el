(require 'cl-lib)
(require 'subr-x)

(use-package seq
  :ensure t)

(defconst elisp-support-default-limit 3
  "Default retry count.")

(defvar elisp-support-mode-name "trueflow"
  "Mode name used in messages.")

(defcustom elisp-support-enabled t
  "Whether support is enabled."
  :type 'boolean
  :group 'elisp-support)

(defmacro elisp-support-with-message (label &rest body)
  "Run BODY and emit LABEL before returning the result."
  `(progn
     (message "running %s" ,label)
     ,@body))

(defun elisp-support-run (items)
  "Normalize ITEMS and report the active entries."
  (let ((normalized (seq-filter #'identity items))
        (results nil))
    ;; keep only truthy values
    (dolist (item normalized)
      (push (string-trim item) results))

    (when elisp-support-enabled
      (message "%s" elisp-support-mode-name))

    (nreverse results)))

(ert-deftest elisp-support-run-test ()
  (should (equal (elisp-support-run '(" a " nil "b")) '("a" "b")))
  (should elisp-support-enabled))

(provide 'elisp-support)
