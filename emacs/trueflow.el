;;; trueflow.el --- Semantic code review interface  -*- lexical-binding: t; -*-

;; Copyright (C) 2026  Architecture Team

;; Author: Architecture Team <team@example.com>
;; Keywords: tools, vc, git
;; Package-Requires: ((emacs "26.1") (magit-section "3.0.0") (transient "0.3.0"))
;; Version: 0.1.0

;;; Commentary:

;; Trueflow is a semantic code review system designed for the agent era.
;; This package provides the Emacs frontend, similar to Magit.

;;; Code:

(require 'magit-section)
(require 'json)
(require 'transient)
(require 'seq)
(require 'cl-lib)
(require 'eieio)
(require 'subr-x)

(defgroup trueflow nil
  "Semantic code review interface."
  :group 'tools)

(defcustom trueflow-exclude-types '("gap")
  "List of block types to exclude from review (e.g. \"gap\", \"comment\")."
  :type '(repeat string)
  :group 'trueflow)

(defcustom trueflow-executable "trueflow"
  "Path to the trueflow CLI executable."
  :type 'string
  :group 'trueflow)

(defface trueflow-action-face
  '((t :inherit shadow :weight bold))
  "Face for action hints in Trueflow buffers."
  :group 'trueflow)

(defface trueflow-code-face
  '((t :background "#f4f4f4" :foreground "#3c3836"))
  "Face for Trueflow code blocks."
  :group 'trueflow)

(defface trueflow-code-comment-face
  '((t :inherit trueflow-code-face :foreground "#7c6f64"))
  "Face for comments in Trueflow code blocks."
  :group 'trueflow)

(defface trueflow-code-add-face
  '((t :inherit trueflow-code-face :foreground "#2e7d32" :weight bold))
  "Face for added lines in Trueflow code blocks."
  :group 'trueflow)

(defface trueflow-code-del-face
  '((t :inherit trueflow-code-face :foreground "#c62828" :weight bold))
  "Face for removed lines in Trueflow code blocks."
  :group 'trueflow)

(defface trueflow-code-gutter-face
  '((t :inherit trueflow-code-face :foreground "#928374"))
  "Face for the Trueflow code gutter."
  :group 'trueflow)

(defface trueflow-code-gutter-add-face
  '((t :inherit trueflow-code-gutter-face :foreground "#2e7d32" :weight bold))
  "Face for the Trueflow add gutter."
  :group 'trueflow)

(defface trueflow-code-gutter-del-face
  '((t :inherit trueflow-code-gutter-face :foreground "#c62828" :weight bold))
  "Face for the Trueflow delete gutter."
  :group 'trueflow)

;;; Classes

(defclass trueflow-file (magit-section) ())
(defclass trueflow-block (magit-section) ())

;;; Keymaps

(defvar trueflow-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "g") #'trueflow-refresh)
    (define-key map (kbd "q") #'quit-window)
    (define-key map (kbd "r") #'trueflow-review-start)
    (define-key map (kbd "a") #'trueflow-approve-at-point)
    (define-key map (kbd "c") #'trueflow-comment-at-point)
    (define-key map (kbd "x") #'trueflow-reject-at-point)
    (define-key map (kbd "e") #'trueflow-edit-at-point)
    (define-key map (kbd "RET") #'trueflow-visit-at-point)
    (define-key map (kbd "?") #'trueflow-dispatch)
    map)
  "Keymap for `trueflow-mode'.")

(defvar magit-trueflow-file-section-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'trueflow-visit-at-point)
    (define-key map (kbd "r") #'trueflow-review-start)
    map)
  "Keymap for file sections.")

(defvar magit-trueflow-block-section-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "RET") #'trueflow-visit-at-point)
    (define-key map (kbd "a") #'trueflow-approve-at-point)
    (define-key map (kbd "x") #'trueflow-reject-at-point)
    (define-key map (kbd "c") #'trueflow-comment-at-point)
    (define-key map (kbd "r") #'trueflow-review-start)
    (define-key map (kbd "s") #'trueflow-subdivide-at-point)
    map)
  "Keymap for block sections.")

(define-derived-mode trueflow-mode magit-section-mode "Trueflow"
  "Major mode for reviewing Trueflow status."
  (hack-dir-local-variables-non-file-buffer))

(transient-define-prefix trueflow-dispatch ()
  "Trueflow actions."
  ["Actions"
   ("r" "Review (Focus)" trueflow-review-start)
   ("g" "Refresh" trueflow-refresh)
   ("q" "Quit" quit-window)]
  ["Block Actions"
   ("a" "Approve" trueflow-approve-at-point)
   ("x" "Reject" trueflow-reject-at-point)
   ("c" "Comment" trueflow-comment-at-point)
   ("e" "Edit/Visit" trueflow-edit-at-point)])

;;; Core Helpers

(defun trueflow-root (&optional dir)
  "Return the root directory of the current repository."
  (let* ((dir (file-name-as-directory (expand-file-name (or dir default-directory))))
         (root (or (let ((default-directory dir))
                     (vc-root-dir))
                   (and (fboundp 'magit-toplevel)
                        (ignore-errors (magit-toplevel dir)))
                   (locate-dominating-file dir ".git"))))
    (if root
        (file-name-as-directory (expand-file-name root))
      (error "Not inside a VCS repository"))))

(defun trueflow--resolve-executable ()
  "Return an absolute path to the trueflow executable."
  (let* ((user-val (and trueflow-executable (expand-file-name trueflow-executable)))
         (root (condition-case nil (trueflow-root) (error nil)))
         (repo-binary (and root (expand-file-name "trueflow/target/debug/trueflow" root))))
    
    (cond
     ((and user-val (file-exists-p user-val) (not (file-directory-p user-val)) (file-executable-p user-val)) 
      user-val)
     ((and repo-binary (file-exists-p repo-binary) (not (file-directory-p repo-binary)) (file-executable-p repo-binary)) 
      repo-binary)
     ((executable-find "trueflow"))
     (t (error "Could not find 'trueflow' binary.")))))

(defun trueflow--ensure-list (vec-or-list)
  "Ensure VEC-OR-LIST is a list."
  (if (vectorp vec-or-list)
      (append vec-or-list nil)
    vec-or-list))

(defconst trueflow--gutter-left 4)
(defconst trueflow--gutter-right 2)

(defun trueflow--short-hash (hash)
  "Return a shortened HASH for display."
  (if (and hash (> (length hash) 8))
      (substring hash 0 8)
    hash))

(defun trueflow--path-from-root (path)
  "Return PATH relative to the repository root."
  (let* ((root (trueflow-root))
         (clean (string-remove-prefix "./" (or path "")))
         (absolute (expand-file-name clean root)))
    (if (string-empty-p clean)
        "<unknown>"
      (file-relative-name absolute root))))

(defun trueflow--fetch-subblocks (block)
  "Return sub-blocks for BLOCK or nil on failure."
  (condition-case nil
      (trueflow--run-command
       (list "inspect" "--fingerprint" (alist-get 'hash block) "--split"))
    (error nil)))

(defun trueflow--match-first (regex content)
  "Return the first capture group for REGEX in CONTENT."
  (when (and content (string-match regex content))
    (match-string 1 content)))

(defun trueflow--block-display-name (block)
  "Return a human-readable display name for BLOCK."
  (let* ((kind (or (alist-get 'kind block) "code"))
         (content (alist-get 'content block))
         (start (1+ (or (alist-get 'start_line block) 0)))
         (end (or (alist-get 'end_line block) start))
         (line-label (format "L%d-L%d" start end))
         (name (cond
                ((string-equal kind "function")
                 (or (trueflow--match-first "\\_<fn\\_>\\s-+\\([[:word:]_]+\\)" content)
                     (trueflow--match-first "\\_<def\\_>\\s-+\\([[:word:]_]+\\)" content)
                     (trueflow--match-first "\\_<function\\_>\\s-+\\([[:word:]_$]+\\)" content)))
                ((string-equal kind "class")
                 (or (trueflow--match-first "\\_<class\\_>\\s-+\\([[:word:]_]+\\)" content)
                     (trueflow--match-first "\\_<struct\\_>\\s-+\\([[:word:]_]+\\)" content)))
                ((string-equal kind "struct")
                 (trueflow--match-first "\\_<struct\\_>\\s-+\\([[:word:]_]+\\)" content))
                ((string-equal kind "enum")
                 (trueflow--match-first "\\_<enum\\_>\\s-+\\([[:word:]_]+\\)" content))
                ((string-equal kind "CodeParagraph") line-label)
                ((string-equal kind "code") line-label)
                ((string-equal kind "TextBlock") line-label))))
    (or name line-label)))

(defun trueflow--subblock-labels (block subblocks)
  "Return display labels for SUBBLOCKS."
  (let* ((labels (mapcar (lambda (sb) (or (alist-get 'kind sb) "code")) subblocks))
         (kind (alist-get 'kind block)))
    (when (and labels (string-equal kind "function"))
      (setf (car labels) "Signature"))
    labels))

(defun trueflow--tree-lines (labels)
  "Return tree lines for LABELS." 
  (if (null labels)
      (list "└─ (none)")
    (let* ((display (if (> (length labels) 4)
                        (append (seq-take labels 2)
                                (list "...")
                                (last labels 2))
                      labels))
           (last-index (1- (length display))))
      (cl-loop for label in display
               for idx from 0
               collect (format "%s %s" (if (= idx last-index) "└─" "├─") label)))))

(defun trueflow--review-header-lines (block subblocks)
  "Return header lines for BLOCK and SUBBLOCKS." 
  (let* ((kind (or (alist-get 'kind block) "code"))
         (name (trueflow--block-display-name block))
         (path (trueflow--path-from-root (alist-get 'file block)))
         (hash (trueflow--short-hash (alist-get 'hash block)))
         (name-part (if (and name (not (string-empty-p name)))
                        (concat " " name)
                      ""))
         (header (format "%s%s in %s (hash=%s), subblocks:" kind name-part path hash))
         (labels (trueflow--subblock-labels block subblocks)))
    (cons header (trueflow--tree-lines labels))))

(defun trueflow--insert-review-header (block subblocks)
  "Insert the review header for BLOCK and SUBBLOCKS." 
  (let ((lines (trueflow--review-header-lines block subblocks)))
    (when lines
      (insert (propertize (concat (car lines) "\n") 'face 'magit-section-heading))
      (dolist (line (cdr lines))
        (insert (propertize (concat line "\n") 'face 'shadow))))))

(defun trueflow--insert-content (content)
  "Insert CONTENT with diff-style highlights."
  (let* ((lines (split-string content "\n" nil))
         (count (length lines))
         (ends-with-newline (string-suffix-p "\n" content)))
    (cl-loop for line in lines
             for idx from 0
             do (trueflow--insert-content-line line)
             do (when (or (< idx (1- count)) ends-with-newline)
                  (insert "\n")))))

(defun trueflow--insert-content-line (line)
  "Insert LINE with diff-style highlights."
  (let* ((gutter (lambda (symbol face)
                   (insert (propertize
                            (concat (make-string trueflow--gutter-left ?\s)
                                    (if symbol (char-to-string symbol) " ")
                                    (make-string trueflow--gutter-right ?\s))
                            'face face))))
         (comment-line (lambda (text)
                         (let ((trimmed (string-trim-left text)))
                           (or (string-prefix-p "//" trimmed)
                               (string-prefix-p "#" trimmed)
                               (string-prefix-p "/*" trimmed)
                               (string-prefix-p "*" trimmed))))))
    (cond
     ((string-prefix-p "+" line)
      (funcall gutter ?+ 'trueflow-code-gutter-add-face)
      (insert (propertize (substring line 1)
                          'face 'trueflow-code-add-face)))
     ((string-prefix-p "-" line)
      (funcall gutter ?- 'trueflow-code-gutter-del-face)
      (insert (propertize (substring line 1)
                          'face 'trueflow-code-del-face)))
     (t
      (funcall gutter nil 'trueflow-code-gutter-face)
      (insert (propertize line 'face (if (funcall comment-line line)
                                         'trueflow-code-comment-face
                                       'trueflow-code-face)))))))

(defun trueflow--insert-actions (text)
  "Insert TEXT near the bottom of the window."
  (let* ((height (window-body-height))
         (current-lines (count-lines (point-min) (point-max)))
         (offset (max 1 (floor (* height 0.1))))
         (target (max current-lines (- height offset)))
         (padding (max 1 (- target current-lines))))
    (insert (make-string padding ?\n))
    (insert (propertize text 'face 'trueflow-action-face))))

(defun trueflow--run-command (args &optional async)
  "Run trueflow command with ARGS."
  (let ((default-directory (trueflow-root))
        (exe (trueflow--resolve-executable)))
    (with-temp-buffer
      (let* ((call-args (append (list exe nil (list t "*trueflow-log*") nil) args))
             (exit-code (apply #'call-process call-args)))
        (unless (zerop exit-code)
          (error "Trueflow command failed: %s %s" (mapconcat #'identity args " ") (buffer-string)))
        (goto-char (point-min))
        (if (looking-at-p "\\[\\|{")
            (trueflow--ensure-list (json-parse-buffer :object-type 'alist :array-type 'list))
          (buffer-string))))))

(defun trueflow--run-mark (hash verdict note)
  "Execute the mark command."
  (let ((args (list "mark" "--fingerprint" hash "--verdict" verdict)))
    (when note
      (setq args (append args (list "--note" note))))
    (trueflow--run-command args)))

(defun trueflow--section-children (section)
  "Return children of SECTION, robustly handling Magit versions."
  (cond
   ((fboundp 'magit-section-children)
    (magit-section-children section))
   ((fboundp 'oref)
    (oref section children))
   (t (error "Cannot determine section children accessor"))))

(defun trueflow--section-value (section)
  "Return value of SECTION."
  (cond
   ((fboundp 'magit-section-value)
    (magit-section-value section))
   ((fboundp 'oref)
    (oref section value))
   (t (error "Cannot determine section value accessor"))))

;;; Status View

;;;###autoload
(defun trueflow-status ()
  "Show the status of Trueflow reviews."
  (interactive)
  (let* ((root (trueflow-root))
         (buf (get-buffer-create (format "*trueflow-status: %s*" (file-name-nondirectory (directory-file-name root))))))
    (with-current-buffer buf
      (trueflow-mode)
      (setq-local default-directory root)
      (trueflow-refresh))
    (switch-to-buffer buf)))

(defun trueflow-refresh ()
  "Refresh the current Trueflow status buffer."
  (interactive)
  (let ((inhibit-read-only t))
    (erase-buffer)
    (save-excursion
      (setq magit-root-section
            (magit-insert-section (status)
              (magit-insert-heading (format "Trueflow Status: %s" (file-name-nondirectory (directory-file-name default-directory))))
              (insert "\n")
              (trueflow--insert-unreviewed-files))))
    (magit-section-update-highlight)))

(defun trueflow--insert-unreviewed-files ()
  "Insert unreviewed files tree."
  (let ((args (list "review" "--json")))
    (dolist (type trueflow-exclude-types)
      (setq args (append args (list "--exclude" type))))
    
    (let ((files (trueflow--run-command args)))
      (setq trueflow-last-scan-data files)
      (if (seq-empty-p files)
          (insert "  All clear! No unreviewed changes found.\n")
        (magit-insert-section (unreviewed)
          (magit-insert-heading (format "Unreviewed Files (%d)" (length files)))
          (seq-doseq (file files)
            (trueflow--insert-file file))
          (insert "\n"))))))

(defun trueflow--insert-file (file-struct)
  "Insert a single FILE-STRUCT section."
  (let ((path (alist-get 'path file-struct))
        (blocks (trueflow--ensure-list (alist-get 'blocks file-struct))))
    (magit-insert-section (trueflow-file path t :type 'trueflow-file)
      (magit-insert-heading (format "%s (%d blocks)" path (length blocks)))
      (magit-insert-section-body
        (seq-doseq (block blocks)
          (trueflow--insert-block path block))))))

(defun trueflow--insert-block (path block-struct)
  "Insert a single BLOCK-STRUCT section for PATH."
  (let ((hash (alist-get 'hash block-struct))
        (content (alist-get 'content block-struct))
        (start (alist-get 'start_line block-struct))
        (end (alist-get 'end_line block-struct))
        (kind (alist-get 'kind block-struct)))
    (magit-insert-section (trueflow-block block-struct :type 'trueflow-block)
      (magit-insert-heading
       (format "L%d-L%d (%s...)" start end (trueflow--short-hash hash)))
      (magit-insert-section-body
        (trueflow--insert-content content)
        (insert "\n")))))

;;; Commands

(defun trueflow-approve-at-point ()
  "Approve the block at point."
  (interactive)
  (trueflow--mark-at-point "approved" nil))

(defun trueflow-reject-at-point ()
  "Reject the block at point."
  (interactive)
  (trueflow--mark-at-point "rejected" nil))

(defun trueflow-comment-at-point ()
  "Comment on the block at point."
  (interactive)
  (let ((section (magit-current-section)))
    (unless (cl-typep section 'trueflow-block)
      (user-error "Not on a block"))
    (let* ((block (trueflow--section-value section))
           (hash (alist-get 'hash block))
           (status-buf (current-buffer))
           (buf (get-buffer-create "*trueflow-comment*")))
      (pop-to-buffer buf '((display-buffer-below-selected) (window-height . 10)))
      (trueflow-comment-mode)
      (erase-buffer)
      (setq-local trueflow-comment-target-hash hash)
      (setq-local trueflow-comment-status-buffer status-buf)
      (insert "# Write your comment below. Press C-c C-c to finish, C-c C-k to cancel.\n\n"))))

(defun trueflow--mark-at-point (verdict note)
  "Mark block at point with VERDICT and NOTE."
  (let ((section (magit-current-section)))
    (unless (cl-typep section 'trueflow-block)
      (user-error "Not on a block"))
    (let* ((block (trueflow--section-value section))
           (hash (alist-get 'hash block)))
      (trueflow--run-mark hash verdict note)
      (trueflow-refresh))))

(defun trueflow-edit-at-point ()
  "Jump to the file location of the block at point."
  (interactive)
  (let ((section (magit-current-section)))
    (cond
     ((cl-typep section 'trueflow-block)
      (let* ((block (trueflow--section-value section))
             (file-section (oref section parent)) ;; Fallback to oref for parent of block
             (path (trueflow--section-value file-section))
             (start (alist-get 'start_line block)))
        (find-file path)
        (goto-char (point-min))
        (forward-line start)))
     ((cl-typep section 'trueflow-file)
      (find-file (trueflow--section-value section)))
     (t (user-error "Not on a file or block")))))

(defun trueflow-visit-at-point ()
  "Visit the file/block at point."
  (interactive)
  (trueflow-edit-at-point))

(defun trueflow-subdivide-at-point ()
  "Subdivide the block at point into smaller sub-blocks."
  (interactive)
  (let ((section (magit-current-section)))
    (when (cl-typep section 'trueflow-block)
      (let ((block (trueflow--section-value section)))
        (trueflow-focus-open block (current-buffer))
        (trueflow-focus-subdivide)))))

;;; Focus Mode

(defvar trueflow-focus-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "a") #'trueflow-focus-approve)
    (define-key map (kbd "x") #'trueflow-focus-reject)
    (define-key map (kbd "c") #'trueflow-focus-comment)
    (define-key map (kbd "n") #'trueflow-focus-next)
    (define-key map (kbd "p") #'trueflow-focus-prev)
    (define-key map (kbd "s") #'trueflow-focus-subdivide)
    (define-key map (kbd "q") #'quit-window)
    (define-key map (kbd "?") #'trueflow-dispatch)
    map)
  "Keymap for Trueflow Focus mode.")

(define-derived-mode trueflow-focus-mode special-mode "Trueflow Focus"
  "Mode for focused review of a single block."
  (hack-dir-local-variables-non-file-buffer))

(defvar-local trueflow-focus-current-block nil)
(defvar-local trueflow-focus-status-buffer nil)
(defvar-local trueflow-focus-subblocks nil)
(defvar-local trueflow-review-items nil
  "List of all blocks to review.")
(defvar-local trueflow-review-index 0
  "Current index in `trueflow-review-items`.")
(defvar-local trueflow-last-scan-data nil
  "The last data retrieved from trueflow CLI.")

(defun trueflow-review-start ()
  "Start a review session from the status buffer."
  (interactive)
  (unless (eq major-mode 'trueflow-mode)
    (user-error "Not in trueflow-status buffer"))
  
  (setq trueflow-review-items
        (seq-mapcat
         (lambda (file)
           (let ((path (alist-get 'path file)))
             (mapcar
              (lambda (block)
                (let ((annotated (copy-alist block)))
                  (setf (alist-get 'file annotated) path)
                  annotated))
              (trueflow--ensure-list (alist-get 'blocks file)))))
         trueflow-last-scan-data))
  (setq trueflow-review-index 0)
  
  (if trueflow-review-items
      (trueflow-focus-update)
    (message "No blocks to review.")))

(defun trueflow-focus-update ()
  "Focus the block at `trueflow-review-index`."
  (let ((status-buf (if (eq major-mode 'trueflow-mode) (current-buffer) trueflow-focus-status-buffer)))
    (if (and status-buf (buffer-live-p status-buf))
        (with-current-buffer status-buf
          (let ((block (nth trueflow-review-index trueflow-review-items))
                (total (length trueflow-review-items)))
            (if block
                (trueflow-focus-open block status-buf (1+ trueflow-review-index) total)
              (message "Review complete! All blocks processed."))))
      (message "Status buffer lost."))))

(defun trueflow-focus-next ()
  "Move to the next block."
  (interactive)
  (with-current-buffer trueflow-focus-status-buffer
    (when (< trueflow-review-index (1- (length trueflow-review-items)))
      (cl-incf trueflow-review-index)
      (trueflow-focus-update))))

(defun trueflow-focus-prev ()
  "Move to the previous block."
  (interactive)
  (with-current-buffer trueflow-focus-status-buffer
    (when (> trueflow-review-index 0)
      (cl-decf trueflow-review-index)
      (trueflow-focus-update))))

(defun trueflow-focus-open (block status-buf current-idx total-count)
  "Open the focus buffer for BLOCK. CURRENT-IDX and TOTAL-COUNT show progress."
  (let ((buf (get-buffer-create "*trueflow-focus*")))
    (switch-to-buffer buf)
    (trueflow-focus-mode)
    (setq trueflow-focus-current-block block)
    (setq trueflow-focus-subblocks (trueflow--fetch-subblocks block))
    (setq trueflow-focus-status-buffer status-buf)
    
    ;; Visual centering/margin
    (let* ((width (window-width))
           (content-width 80) ;; Target width for code
           (margin (max 0 (/ (- width content-width) 2))))
      (set-window-margins nil margin margin))

    (let ((inhibit-read-only t))
      (erase-buffer)
      (trueflow--insert-review-header block trueflow-focus-subblocks)
      (insert "\n\n")
      (trueflow--insert-content (alist-get 'content block))
      (trueflow--insert-actions
       (format "Actions: [a]pprove  [x]reject  [c]omment  [n]ext  [p]rev  [q]uit  (%d / %d)" current-idx total-count)))
    (goto-char (point-min))))

(defun trueflow-focus-subdivide ()
  "Subdivide the current block into sub-blocks and display them."
  (interactive)
  (when trueflow-focus-current-block
    (let* ((hash (alist-get 'hash trueflow-focus-current-block))
           (sub-blocks (or trueflow-focus-subblocks
                           (trueflow--run-command (list "inspect" "--fingerprint" hash "--split")))))
      (setq trueflow-focus-subblocks sub-blocks)
      (let ((inhibit-read-only t))
        (erase-buffer)
        (trueflow--insert-review-header trueflow-focus-current-block sub-blocks)
        (insert "\n\n")
        
        (magit-insert-section (sub-blocks)
          (seq-doseq (block sub-blocks)
             (trueflow--insert-block nil block)))
        
        (trueflow--insert-actions
         "Actions: [a]pprove  [x]reject  [c]omment  [n]ext  [p]rev  [q]uit")
        (goto-char (point-min))))))

(defun trueflow-focus-action (verdict &optional note)
  "Apply VERDICT to current focused block and move to next."
  (when trueflow-focus-current-block
    (let ((hash (alist-get 'hash trueflow-focus-current-block)))
      (trueflow--run-mark hash verdict note)
      (with-current-buffer trueflow-focus-status-buffer
        (trueflow-refresh)
        (trueflow-focus-next)))))

(defun trueflow-focus-approve ()
  (interactive)
  (let ((section (magit-current-section)))
    (if (cl-typep section 'trueflow-block)
        (trueflow--mark-at-point "approved" nil)
      (trueflow-focus-action "approved"))))

(defun trueflow-focus-reject ()
  (interactive)
  (let ((section (magit-current-section)))
    (if (cl-typep section 'trueflow-block)
        (trueflow--mark-at-point "rejected" nil)
      (trueflow-focus-action "rejected"))))

(defun trueflow-focus-comment ()
  (interactive)
  (when trueflow-focus-current-block
    (let* ((block trueflow-focus-current-block)
           (hash (alist-get 'hash block))
           (status-buf trueflow-focus-status-buffer)
           (buf (get-buffer-create "*trueflow-comment*")))
      (pop-to-buffer buf '((display-buffer-below-selected) (window-height . 10)))
      (trueflow-comment-mode)
      (erase-buffer)
      (setq-local trueflow-comment-target-hash hash)
      (setq-local trueflow-comment-status-buffer status-buf)
      (setq-local trueflow-comment-after-commit-function 
                  (lambda ()
                    (with-current-buffer status-buf
                      (trueflow-refresh)
                      (trueflow-focus-next))))
      (insert "# Write your comment below. Press C-c C-c to finish, C-c C-k to cancel.\n\n"))))

(defun trueflow-focus-skip ()
  (interactive)
  (trueflow-focus-next))

;;; Comment Mode

(defvar trueflow-comment-mode-map
  (let ((map (make-sparse-keymap)))
    (define-key map (kbd "C-c C-c") #'trueflow-comment-commit)
    (define-key map (kbd "C-c C-k") #'trueflow-comment-abort)
    map)
  "Keymap for `trueflow-comment-mode'.")

(define-derived-mode trueflow-comment-mode text-mode "Trueflow Comment"
  "Major mode for writing Trueflow comments.
Press C-c C-c to commit, C-c C-k to cancel."
  :group 'trueflow)

(defvar-local trueflow-comment-target-hash nil
  "The hash of the block being commented on.")

(defvar-local trueflow-comment-status-buffer nil
  "The buffer to return to after commenting.")

(defvar-local trueflow-comment-after-commit-function nil
  "Optional function to call after committing a comment.")

(defun trueflow-comment-commit ()
  "Submit the comment in the current buffer."
  (interactive)
  (unless trueflow-comment-target-hash
    (error "Not in a valid trueflow comment buffer"))
  (let ((content (buffer-substring-no-properties (point-min) (point-max))))
    (setq content (replace-regexp-in-string "^#.*\n" "" content))
    (setq content (string-trim content))
    (if (string-empty-p content)
        (message "Comment aborted (empty)")
      (trueflow--run-mark trueflow-comment-target-hash "comment" content)
      (message "Comment recorded"))
    (quit-window t)
    (if trueflow-comment-after-commit-function
        (funcall trueflow-comment-after-commit-function)
      (when (buffer-live-p trueflow-comment-status-buffer)
        (with-current-buffer trueflow-comment-status-buffer
          (trueflow-refresh))))))

(defun trueflow-comment-abort ()
  "Abort commenting."
  (interactive)
  (message "Comment aborted")
  (quit-window t))

(provide 'trueflow)

;;; Evil Mode Compatibility
(when (fboundp 'evil-set-initial-state)
  (evil-set-initial-state 'trueflow-mode 'emacs)
  (evil-set-initial-state 'trueflow-focus-mode 'emacs))

;;; trueflow.el ends here
