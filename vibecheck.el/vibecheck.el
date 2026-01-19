;;; vibecheck.el --- Semantic code review interface  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Architecture Team

;; Author: Architecture Team <team@example.com>
;; Keywords: tools, vc, git
;; Package-Requires: ((emacs "26.1") (magit-section "3.0.0") (transient "0.3.0"))
;; Version: 0.1.0

;;; Commentary:

;; Vibecheck is a semantic code review system designed for the agent era.
;; This package provides the Emacs frontend, similar to Magit.

;;; Code:

(require 'magit-section)
(require 'json)
(require 'transient)

(defgroup vibecheck nil
  "Semantic code review interface."
  :group 'tools)

(defcustom vibecheck-executable "vibecheck"
  "Path to the vibecheck CLI executable."
  :type 'string
  :group 'vibecheck)

(defvar vibecheck-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "g") #'vibecheck-refresh)
    (define-key map (kbd "q") #'quit-window)
    (define-key map (kbd "a") #'vibecheck-approve-at-point)
    (define-key map (kbd "c") #'vibecheck-comment-at-point)
    (define-key map (kbd "x") #'vibecheck-reject-at-point)
    (define-key map (kbd "e") #'vibecheck-edit-at-point)
    (define-key map (kbd "RET") #'vibecheck-visit-at-point)
    map)
  "Keymap for `vibecheck-mode'.")

(define-derived-mode vibecheck-mode magit-section-mode "Vibecheck"
  "Major mode for reviewing Vibecheck status."
  :group 'vibecheck
  (hack-dir-local-variables-non-file-buffer))

(defun vibecheck-root ()
  "Return the root directory of the current repository."
  (or (vc-root-dir)
      (error "Not inside a VC repository")))

(defun vibecheck--run-command (args &optional async)
  "Run vibecheck command with ARGS.
If ASYNC is non-nil, run asynchronously and ignore output.
Returns parsed JSON if output is JSON, or string otherwise."
  (let ((default-directory (vibecheck-root)))
    (with-temp-buffer
      (let ((exit-code (apply #'call-process vibecheck-executable nil t nil args)))
        (unless (zerop exit-code)
          (error "Vibecheck command failed: %s %s" (mapconcat #'identity args " ") (buffer-string)))
        (goto-char (point-min))
        (if (looking-at-p "\\[\\|{")
            (json-parse-buffer :object-type 'alist)
          (buffer-string))))))

;;;###autoload
(defun vibecheck-status ()
  "Show the status of Vibecheck reviews."
  (interactive)
  (let* ((root (vibecheck-root))
         (buf (get-buffer-create (format "*vibecheck-status: %s*" (file-name-nondirectory (directory-file-name root))))))
    (with-current-buffer buf
      (vibecheck-mode)
      (setq-local default-directory root)
      (vibecheck-refresh))
    (switch-to-buffer buf)))

(defun vibecheck-refresh ()
  "Refresh the current Vibecheck status buffer."
  (interactive)
  (let ((inhibit-read-only t))
    (erase-buffer)
    (save-excursion
      (magit-insert-section (vibecheck-root)
        (magit-insert-heading (format "Vibecheck Status: %s" (file-name-nondirectory (directory-file-name default-directory))))
        (insert "\n")
        (vibecheck--insert-unreviewed-changes)))
    (magit-section-update-highlight)))

(defun vibecheck--insert-unreviewed-changes ()
  "Insert unreviewed changes section."
  (let ((changes (vibecheck--run-command '("diff" "--json"))))
    (if (seq-empty-p changes)
        (insert "  All clear! No unreviewed changes.\n")
      (magit-insert-section (unreviewed)
        (magit-insert-heading (format "Unreviewed Changes (%d)" (length changes)))
        (dolist (change changes)
          (vibecheck--insert-change change))
        (insert "\n")))))

(defun vibecheck--insert-change (change)
  "Insert a single CHANGE."
  (let ((file (alist-get 'file change))
        (line (alist-get 'line change))
        (fp (alist-get 'fingerprint change))
        (diff-content (alist-get 'diff_content change))
        (new-content (alist-get 'new_content change))
        (context (alist-get 'context change)))
    (magit-insert-section (change change)
      (magit-insert-heading (format "%s:%d  (fp: %s...)" file line (substring fp 0 8)))
      (magit-insert-section-body
        (insert (propertize "Context:\n" 'face 'shadow))
        (insert (propertize context 'face 'shadow))
        (insert (propertize "\nDiff:\n" 'face 'magit-diff-hunk-heading))
        (insert (propertize diff-content 'face 'font-lock-string-face))
        (insert (propertize "\n\nNew Content (Clean):\n" 'face 'magit-diff-hunk-heading))
        (insert (propertize new-content 'face 'default))
        (insert "\n\n")))))

(defun vibecheck-approve-at-point ()
  "Approve the hunk at point."
  (interactive)
  (vibecheck--mark-at-point "approved" nil))

(defun vibecheck-reject-at-point ()
  "Reject the hunk at point."
  (interactive)
  (vibecheck--mark-at-point "rejected" nil))

(defun vibecheck-comment-at-point ()
  "Comment on the hunk at point."
  (interactive)
  (let ((note (read-string "Comment: ")))
    (vibecheck--mark-at-point "comment" note)))

(defun vibecheck--mark-at-point (verdict note)
  "Mark change at point with VERDICT and NOTE."
  (let ((section (magit-current-section)))
    (unless (eq (magit-section-type section) 'change)
      (user-error "Not on a change"))
    (let* ((change (magit-section-value section))
           (fp (alist-get 'fingerprint change)))
      (let ((args (list "mark" "--fingerprint" fp "--verdict" verdict)))
        (when note
          (setq args (append args (list "--note" note))))
        (vibecheck--run-command args)
        (vibecheck-refresh)))))

(defun vibecheck-edit-at-point ()
  "Jump to the file location of the change at point."
  (interactive)
  (let ((section (magit-current-section)))
    (unless (eq (magit-section-type section) 'change)
      (user-error "Not on a change"))
    (let* ((change (magit-section-value section))
           (file (alist-get 'file change))
           (line (alist-get 'line change)))
      (find-file file)
      (goto-char (point-min))
      (forward-line (1- line)))))

(defun vibecheck-visit-at-point ()
  "Visit the file for the hunk at point."
  (interactive)
  (vibecheck-edit-at-point))

(provide 'vibecheck)
;;; vibecheck.el ends here
