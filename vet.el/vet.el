;;; vet.el --- Semantic code review interface  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Architecture Team

;; Author: Architecture Team <team@example.com>
;; Keywords: tools, vc, git
;; Package-Requires: ((emacs "26.1") (magit-section "3.0.0") (transient "0.3.0"))
;; Version: 0.1.0

;;; Commentary:

;; Vet is a semantic code review system designed for the agent era.
;; This package provides the Emacs frontend, similar to Magit.

;;; Code:

(require 'magit-section)
(require 'json)
(require 'transient)

(defgroup vet nil
  "Semantic code review interface."
  :group 'tools)

(defcustom vet-executable "vet"
  "Path to the vet CLI executable."
  :type 'string
  :group 'vet)

(defvar vet-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "g") #'vet-refresh)
    (define-key map (kbd "q") #'quit-window)
    (define-key map (kbd "a") #'vet-approve-at-point)
    (define-key map (kbd "c") #'vet-comment-at-point)
    (define-key map (kbd "x") #'vet-reject-at-point)
    (define-key map (kbd "e") #'vet-edit-at-point)
    (define-key map (kbd "RET") #'vet-visit-at-point)
    map)
  "Keymap for `vet-mode'.")

(define-derived-mode vet-mode magit-section-mode "Vet"
  "Major mode for reviewing Vet status."
  :group 'vet
  (hack-dir-local-variables-non-file-buffer))

(defun vet-root ()
  "Return the root directory of the current repository."
  (or (vc-root-dir)
      (error "Not inside a VC repository")))

(defun vet--run-command (args &optional async)
  "Run vet command with ARGS.
If ASYNC is non-nil, run asynchronously and ignore output.
Returns parsed JSON if output is JSON, or string otherwise."
  (let ((default-directory (vet-root)))
    (with-temp-buffer
      (let ((exit-code (apply #'call-process vet-executable nil t nil args)))
        (unless (zerop exit-code)
          (error "Vet command failed: %s %s" (mapconcat #'identity args " ") (buffer-string)))
        (goto-char (point-min))
        (if (looking-at-p "\\[\\|{") ;; simplistic JSON detection
            (json-parse-buffer :object-type 'alist)
          (buffer-string))))))

;;;###autoload
(defun vet-status ()
  "Show the status of Vet reviews."
  (interactive)
  (let* ((root (vet-root))
         (buf (get-buffer-create (format "*vet-status: %s*" (file-name-nondirectory (directory-file-name root))))))
    (with-current-buffer buf
      (vet-mode)
      (setq-local default-directory root)
      (vet-refresh))
    (switch-to-buffer buf)))

(defun vet-refresh ()
  "Refresh the current Vet status buffer."
  (interactive)
  (let ((inhibit-read-only t))
    (erase-buffer)
    (save-excursion
      (magit-insert-section (vet-root)
        (magit-insert-heading (format "Vet Status: %s" (file-name-nondirectory (directory-file-name default-directory))))
        (insert "\n")
        (vet--insert-unreviewed-changes)))
    (magit-section-update-highlight)))

(defun vet--insert-unreviewed-changes ()
  "Insert unreviewed changes section."
  (let ((changes (vet--run-command '("diff" "--json"))))
    (if (seq-empty-p changes)
        (insert "  All clear! No unreviewed changes.\n")
      (magit-insert-section (unreviewed)
        (magit-insert-heading (format "Unreviewed Changes (%d)" (length changes)))
        (dolist (change changes)
          (vet--insert-change change))
        (insert "\n")))))

(defun vet--insert-change (change)
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

(defun vet-approve-at-point ()
  "Approve the hunk at point."
  (interactive)
  (vet--mark-at-point "approved" nil))

(defun vet-reject-at-point ()
  "Reject the hunk at point."
  (interactive)
  (vet--mark-at-point "rejected" nil))

(defun vet-comment-at-point ()
  "Comment on the hunk at point."
  (interactive)
  (let ((note (read-string "Comment: ")))
    (vet--mark-at-point "comment" note)))

(defun vet--mark-at-point (verdict note)
  "Mark change at point with VERDICT and NOTE."
  (let ((section (magit-current-section)))
    (unless (eq (magit-section-type section) 'change)
      (user-error "Not on a change"))
    (let* ((change (magit-section-value section))
           (fp (alist-get 'fingerprint change)))
      (let ((args (list "mark" "--fingerprint" fp "--verdict" verdict)))
        (when note
          (setq args (append args (list "--note" note))))
        (vet--run-command args)
        (vet-refresh)))))

(defun vet-edit-at-point ()
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

(defun vet-visit-at-point ()
  "Visit the file for the hunk at point."
  (interactive)
  (vet-edit-at-point))

(provide 'vet)
;;; vet.el ends here
