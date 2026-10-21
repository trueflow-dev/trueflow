(require 'ert)
(require 'cl-lib)

(unless (require 'magit-section nil 'noerror)
  (require 'eieio)
  (defclass magit-section () ())
  (define-derived-mode magit-section-mode special-mode "Magit Section")
  (defvar magit-root-section nil)
  (defmacro magit-insert-section (_section &rest body)
    `(progn ,@body))
  (defmacro magit-insert-section-body (&rest body)
    `(progn ,@body))
  (defmacro magit-insert-heading (&rest body)
    `(progn ,@body))
  (defun magit-section-update-highlight () nil)
  (defun magit-current-section () nil)
  (provide 'magit-section))

(require 'trueflow)

(defun trueflow-test--make-executable (dir relative-path)
  "Create an executable file under DIR at RELATIVE-PATH and return its path."
  (let ((path (expand-file-name relative-path dir)))
    (make-directory (file-name-directory path) t)
    (with-temp-file path
      (insert "#!/bin/sh\nexit 0\n"))
    (set-file-modes path #o755)
    path))

(defun trueflow-test--block (hash)
  "Return a minimal review block for HASH."
  `((hash . ,hash)
    (content . ,(format "fn %s() {}\n" hash))
    (start_line . 3)
    (end_line . 3)
    (kind . "function")
    (file . "src/lib.rs")))

(defun trueflow-test--scan-data (hashes)
  "Return scan data containing HASHES in order."
  (list
   `((path . "src/lib.rs")
     (blocks . ,(mapcar #'trueflow-test--block hashes)))))

(defun trueflow-test--review-items (hashes)
  "Return review items annotated like `trueflow-review-start'."
  (mapcar #'trueflow-test--block hashes))

(defun trueflow-test--exercise-focus-action (action-fn)
  "Run ACTION-FN and return (MARK-ARGS OPENED REVIEW-INDEX REVIEW-HASHES)."
  (let* ((initial-hashes '("a" "b" "c"))
         (remaining-hashes '("b" "c"))
         (initial-scan (trueflow-test--scan-data initial-hashes))
         (remaining-scan (trueflow-test--scan-data remaining-hashes))
         (status-buf (generate-new-buffer " *trueflow-test-status*"))
         (focus-buf (generate-new-buffer " *trueflow-test-focus*"))
         mark-args
         opened)
    (unwind-protect
        (progn
          (with-current-buffer status-buf
            (trueflow-mode)
            (setq-local trueflow-last-scan-data initial-scan)
            (setq-local trueflow-review-items
                        (trueflow-test--review-items initial-hashes))
            (setq-local trueflow-review-index 0))
          (with-current-buffer focus-buf
            (trueflow-focus-mode)
            (setq-local trueflow-focus-current-block (trueflow-test--block "a"))
            (setq-local trueflow-focus-status-buffer status-buf))
          (cl-letf (((symbol-function 'magit-current-section) (lambda () nil))
                    ((symbol-function 'trueflow--run-command)
                     (lambda (args)
                       (setq mark-args args)
                       ""))
                    ((symbol-function 'trueflow-refresh)
                     (lambda ()
                       (setq trueflow-last-scan-data remaining-scan)))
                    ((symbol-function 'trueflow--fetch-subblocks) (lambda (&rest _) nil))
                    ((symbol-function 'trueflow-focus-open)
                     (lambda (block _status-buf current-idx total-count)
                       (setq opened (list (alist-get 'hash block) current-idx total-count)))))
            (with-current-buffer focus-buf
              (funcall action-fn)))
          (list mark-args
                opened
                (with-current-buffer status-buf trueflow-review-index)
                (with-current-buffer status-buf
                  (mapcar (lambda (block) (alist-get 'hash block))
                          trueflow-review-items))))
      (kill-buffer status-buf)
      (kill-buffer focus-buf))))


(ert-deftest trueflow-root-falls-back-to-current-directory-outside-vcs ()
  "`trueflow-root' should work outside git/VCS repositories."
  (let* ((dir (file-name-as-directory (make-temp-file "trueflow-root-" t)))
         (default-directory dir))
    (cl-letf (((symbol-function 'vc-root-dir) (lambda () nil))
              ((symbol-function 'magit-toplevel) (lambda (&optional _) nil))
              ((symbol-function 'locate-dominating-file)
               (lambda (&rest _) nil)))
      (should (equal (trueflow-root) dir)))))

(ert-deftest trueflow-resolve-executable-accepts-absolute-paths ()
  "Absolute executable paths should be used directly."
  (let* ((dir (make-temp-file "trueflow-exec-" t))
         (path (trueflow-test--make-executable dir "bin/trueflow"))
         (trueflow-executable path)
         (trueflow-allow-repo-local-executable nil))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) dir))
              ((symbol-function 'executable-find) (lambda (&rest _) nil)))
      (should (equal (trueflow--resolve-executable) path)))))

(ert-deftest trueflow-resolve-executable-accepts-relative-paths-with-separators ()
  "Relative executable paths containing separators should be treated as paths."
  (let* ((dir (file-name-as-directory (make-temp-file "trueflow-exec-" t)))
         (default-directory dir)
         (path (trueflow-test--make-executable dir "bin/trueflow"))
         (trueflow-executable "./bin/trueflow")
         (trueflow-allow-repo-local-executable nil))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) dir))
              ((symbol-function 'executable-find) (lambda (&rest _) nil)))
      (should (equal (trueflow--resolve-executable) path)))))

(ert-deftest trueflow-resolve-executable-uses-executable-find-for-bare-commands ()
  "Bare command names should resolve through `executable-find'."
  (let ((trueflow-executable "trueflow-custom")
        (trueflow-allow-repo-local-executable nil))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) "/tmp/trueflow/"))
              ((symbol-function 'executable-find)
               (lambda (command)
                 (and (equal command "trueflow-custom")
                      "/usr/local/bin/trueflow-custom"))))
      (should (equal (trueflow--resolve-executable)
                     "/usr/local/bin/trueflow-custom")))))

(ert-deftest trueflow-resolve-executable-ignores-repo-local-build-by-default ()
  "Repo-local binaries should not be auto-executed unless explicitly enabled."
  (let* ((root (file-name-as-directory (make-temp-file "trueflow-repo-" t)))
         (_repo-bin (trueflow-test--make-executable root "trueflow/target/debug/trueflow"))
         (default-directory root)
         (trueflow-executable "trueflow")
         (trueflow-allow-repo-local-executable nil))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) root))
              ((symbol-function 'executable-find) (lambda (&rest _) nil)))
      (should-error (trueflow--resolve-executable)))))

(ert-deftest trueflow-resolve-executable-allows-repo-local-build-when-opted-in ()
  "Repo-local binaries should be usable when explicitly enabled."
  (let* ((root (file-name-as-directory (make-temp-file "trueflow-repo-" t)))
         (repo-bin (trueflow-test--make-executable root "trueflow/target/debug/trueflow"))
         (default-directory root)
         (trueflow-executable "trueflow")
         (trueflow-allow-repo-local-executable t))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) root))
              ((symbol-function 'executable-find) (lambda (&rest _) nil)))
      (should (equal (trueflow--resolve-executable) repo-bin)))))

(ert-deftest trueflow-review-header-lines-show-semantic-identity ()
  "Review headers should expose the path, declaration, hash, and subblock kind."
  (let ((block '((hash . "a85ccbc912345678")
                 (content . "#[derive(Debug, Clone)]\nstruct Config {\n    name: String,\n}\n")
                 (start_line . 0)
                 (end_line . 3)
                 (kind . "struct")
                 (file . "./example_repos/all_languages/main.rs")))
        (subblocks '(((kind . "CodeParagraph")))))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) "/repo/")))
      (let* ((lines (trueflow--review-header-lines block subblocks))
             (rendered (mapconcat #'identity lines "\n")))
        (should (string-match-p "example_repos/all_languages/main\\.rs" rendered))
        (should-not (string-match-p "/repo/" rendered))
        (should (string-match-p "struct Config" rendered))
        (should (string-match-p "a85ccbc9" rendered))
        (should (string-match-p "CodeParagraph" rendered))))))

(ert-deftest trueflow-focus-actions-mark-and-refresh-without-skipping-next-item ()
  "Approve and reject should send distinct verdicts and open the next item."
  (dolist (case '((trueflow-focus-approve "approved")
                  (trueflow-focus-reject "rejected")))
    (pcase-let ((`(,mark-args ,opened ,review-index ,review-hashes)
                  (trueflow-test--exercise-focus-action (car case))))
      (should (equal mark-args
                     (list "mark" "--fingerprint" "a" "--verdict" (cadr case)
                           "--path" "src/lib.rs" "--line" "3")))
      (should (equal opened '("b" 1 2)))
      (should (= review-index 0))
      (should (equal review-hashes '("b" "c"))))))

(ert-deftest trueflow-focus-comment-callback-refreshes-without-skipping-next-item ()
  "Comment follow-up should advance to the next refreshed item, not skip it."
  (let* ((initial-hashes '("a" "b" "c"))
         (remaining-hashes '("b" "c"))
         (initial-scan (trueflow-test--scan-data initial-hashes))
         (remaining-scan (trueflow-test--scan-data remaining-hashes))
         (status-buf (generate-new-buffer " *trueflow-test-status*"))
         (focus-buf (generate-new-buffer " *trueflow-test-focus*"))
         (comment-buf nil)
         opened)
    (unwind-protect
        (progn
          (with-current-buffer status-buf
            (trueflow-mode)
            (setq-local trueflow-last-scan-data initial-scan)
            (setq-local trueflow-review-items
                        (trueflow-test--review-items initial-hashes))
            (setq-local trueflow-review-index 0))
          (with-current-buffer focus-buf
            (trueflow-focus-mode)
            (setq-local trueflow-focus-current-block (trueflow-test--block "a"))
            (setq-local trueflow-focus-status-buffer status-buf))
          (cl-letf (((symbol-function 'pop-to-buffer)
                     (lambda (buffer &rest _)
                       (setq comment-buf buffer)
                       (set-buffer buffer)
                       buffer))
                    ((symbol-function 'trueflow-refresh)
                     (lambda ()
                       (setq trueflow-last-scan-data remaining-scan)))
                    ((symbol-function 'trueflow--fetch-subblocks) (lambda (&rest _) nil))
                    ((symbol-function 'trueflow-focus-open)
                     (lambda (block _status-buf current-idx total-count)
                       (setq opened (list (alist-get 'hash block) current-idx total-count)))))
            (with-current-buffer focus-buf
              (trueflow-focus-comment))
            (with-current-buffer comment-buf
              (funcall trueflow-comment-after-commit-function)))
          (should (equal opened '("b" 1 2)))
          (with-current-buffer status-buf
            (should (= trueflow-review-index 0))
            (should (equal (mapcar (lambda (block) (alist-get 'hash block))
                                   trueflow-review-items)
                           '("b" "c")))))
      (when (buffer-live-p comment-buf)
        (kill-buffer comment-buf))
      (kill-buffer status-buf)
      (kill-buffer focus-buf))))

(ert-deftest trueflow-mark-at-point-passes-path-and-line ()
  "Status-buffer marks should identify the reviewed occurrence."
  (let ((block '((hash . "h")
                 (content . "fn h() {}\n")
                 (start_line . 7)
                 (end_line . 7)
                 (kind . "function")
                 (file . "src/a.rs")))
        mark-args)
    (cl-letf (((symbol-function 'magit-current-section)
               (lambda () (make-instance 'trueflow-block)))
              ((symbol-function 'trueflow--section-value) (lambda (_) block))
              ((symbol-function 'trueflow-refresh) #'ignore)
              ((symbol-function 'trueflow--run-command)
               (lambda (args)
                 (setq mark-args args)
                 "")))
      (trueflow-approve-at-point))
    (should (equal mark-args
                   '("mark" "--fingerprint" "h" "--verdict" "approved"
                     "--path" "src/a.rs" "--line" "7")))))

(ert-deftest trueflow-mark-at-point-reviews-a-deleted-block-as-its-deletion ()
  "A block `review --json' lists as deleted is reviewed as its deletion by HEAD's changes."
  (let ((block '((hash . "h")
                 (content . "fn h() {}\n")
                 (start_line . 7)
                 (end_line . 7)
                 (kind . "function")
                 (change . "deleted")
                 (file . "src/a.rs")))
        mark-args)
    (cl-letf (((symbol-function 'magit-current-section)
               (lambda () (make-instance 'trueflow-block)))
              ((symbol-function 'trueflow--section-value) (lambda (_) block))
              ((symbol-function 'trueflow-refresh) #'ignore)
              ((symbol-function 'trueflow--run-command)
               (lambda (args)
                 (setq mark-args args)
                 "")))
      (trueflow-approve-at-point))
    (should (equal mark-args
                   '("mark" "--fingerprint" "h" "--verdict" "approved"
                     "--path" "src/a.rs" "--line" "7" "--deleted-in" "HEAD")))))

(ert-deftest trueflow-review-items-and-sections-share-file-annotation ()
  "Status sections and review items should carry the block's file path."
  (let ((block '((hash . "h") (start_line . 2))))
    (should (equal (alist-get 'file (trueflow--annotate-block "src/a.rs" block))
                   "src/a.rs"))
    (should-not (alist-get 'file block))
    (should (equal (mapcar (lambda (item) (alist-get 'file item))
                           (trueflow--review-items-from-scan-data
                            (trueflow-test--scan-data '("a" "b"))))
                   '("src/lib.rs" "src/lib.rs")))))

(ert-deftest trueflow-subdivide-at-point-opens-focus ()
  "Splitting a status-buffer block should open the focus view on its parts."
  (let ((block '((hash . "parent")
                 (content . "fn parent() {}\n")
                 (start_line . 4)
                 (end_line . 4)
                 (kind . "function")
                 (file . "src/a.rs")))
        (status-buf (generate-new-buffer " *trueflow-test-status*")))
    (unwind-protect
        (cl-letf (((symbol-function 'magit-current-section)
                   (lambda () (make-instance 'trueflow-block)))
                  ((symbol-function 'trueflow--section-value) (lambda (_) block))
                  ((symbol-function 'trueflow-root) (lambda (&optional _) "/repo/"))
                  ((symbol-function 'trueflow--run-command)
                   (lambda (_args)
                     '(((hash . "child")
                        (content . "child_body();\n")
                        (start_line . 4)
                        (end_line . 4)
                        (kind . "CodeParagraph"))))))
          (with-current-buffer status-buf
            (trueflow-mode)
            (trueflow-subdivide-at-point))
          (with-current-buffer "*trueflow-focus*"
            (should (eq trueflow-focus-status-buffer status-buf))
            (should (string-match-p "child_body" (buffer-string)))))
      (when (get-buffer "*trueflow-focus*")
        (kill-buffer "*trueflow-focus*"))
      (kill-buffer status-buf))))

(ert-deftest trueflow-focus-subblock-verdicts-refresh-status-not-focus ()
  "Marking a sub-block in the split view refreshes the status buffer.
The focus buffer keeps showing the split parent instead of a status view."
  (let* ((parent (trueflow-test--block "parent"))
         (child '((hash . "child")
                  (content . "child_body();\n")
                  (start_line . 3)
                  (end_line . 3)
                  (kind . "CodeParagraph")
                  (file . "src/lib.rs")))
         (scan (trueflow-test--scan-data '("parent" "other"))))
    (dolist (case '((trueflow-focus-approve "approved")
                    (trueflow-focus-reject "rejected")))
      (let ((status-buf (generate-new-buffer " *trueflow-test-status*"))
            refreshed-in
            mark-args)
        (unwind-protect
            (cl-letf (((symbol-function 'magit-current-section)
                       (lambda () (make-instance 'trueflow-block)))
                      ((symbol-function 'trueflow--section-value) (lambda (_) child))
                      ((symbol-function 'trueflow-refresh)
                       (lambda ()
                         (setq refreshed-in (current-buffer))
                         (setq trueflow-last-scan-data scan)))
                      ((symbol-function 'trueflow--run-command)
                       (lambda (args)
                         (if (equal (car args) "mark")
                             (progn (setq mark-args args) "")
                           (list child)))))
              (with-current-buffer status-buf
                (trueflow-mode)
                (setq-local trueflow-last-scan-data scan)
                (trueflow-focus-open parent status-buf)
                (trueflow-focus-subdivide))
              (with-current-buffer "*trueflow-focus*"
                (funcall (car case)))
              (should (equal mark-args
                             (list "mark" "--fingerprint" "child" "--verdict" (cadr case)
                                   "--path" "src/lib.rs" "--line" "3")))
              (should (eq refreshed-in status-buf))
              (with-current-buffer "*trueflow-focus*"
                (should (eq major-mode 'trueflow-focus-mode))
                (should (equal (alist-get 'hash trueflow-focus-current-block) "parent"))
                (should (string-match-p "child_body" (buffer-string)))))
          (when (get-buffer "*trueflow-focus*")
            (kill-buffer "*trueflow-focus*"))
          (kill-buffer status-buf))))))

(ert-deftest trueflow-comment-commit-runs-after-commit-callback ()
  "Committing a comment should mark it and refresh the focus session."
  (let* ((status-buf (generate-new-buffer " *trueflow-test-status*"))
         (focus-buf (generate-new-buffer " *trueflow-test-focus*"))
         (remaining-scan (trueflow-test--scan-data '("b" "c")))
         mark-args
         opened)
    (unwind-protect
        (progn
          (with-current-buffer status-buf
            (trueflow-mode)
            (setq-local trueflow-last-scan-data (trueflow-test--scan-data '("a" "b" "c")))
            (setq-local trueflow-review-items (trueflow-test--review-items '("a" "b" "c")))
            (setq-local trueflow-review-index 0))
          (switch-to-buffer focus-buf)
          (trueflow-focus-mode)
          (setq-local trueflow-focus-current-block (trueflow-test--block "a"))
          (setq-local trueflow-focus-status-buffer status-buf)
          (cl-letf (((symbol-function 'trueflow--run-command)
                     (lambda (args)
                       (setq mark-args args)
                       ""))
                    ((symbol-function 'trueflow-refresh)
                     (lambda ()
                       (setq trueflow-last-scan-data remaining-scan)))
                    ((symbol-function 'trueflow-focus-open)
                     (lambda (block _status-buf current-idx total-count)
                       (setq opened (list (alist-get 'hash block) current-idx total-count)))))
            (trueflow-focus-comment)
            (should (eq major-mode 'trueflow-comment-mode))
            (goto-char (point-max))
            (insert "looks off")
            (trueflow-comment-commit))
          (should (equal mark-args
                         '("mark" "--fingerprint" "a" "--verdict" "comment"
                           "--path" "src/lib.rs" "--line" "3" "--note" "looks off")))
          (should (equal opened '("b" 1 2))))
      (when (get-buffer "*trueflow-comment*")
        (kill-buffer "*trueflow-comment*"))
      (kill-buffer status-buf)
      (kill-buffer focus-buf))))

(ert-deftest trueflow-run-command-reports-stderr-without-writing-repo-files ()
  "Failed commands should surface stderr and never write logs into the repo."
  (let* ((root (file-name-as-directory (make-temp-file "trueflow-repo-" t)))
         (exe (expand-file-name "fail.sh" root))
         (trueflow-executable exe))
    (with-temp-file exe
      (insert "#!/bin/sh\necho partial\necho boom >&2\nexit 1\n"))
    (set-file-modes exe #o755)
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) root)))
      (let ((err (should-error (trueflow--run-command '("mark")))))
        (should (string-match-p "boom" (error-message-string err)))
        (should (string-match-p "partial" (error-message-string err)))))
    (should-not (file-exists-p (expand-file-name "*trueflow-log*" root)))))

(ert-deftest trueflow-insert-content-renders-leading-markers-as-source ()
  "Review content is raw source, so leading + and - are ordinary text."
  (with-temp-buffer
    (trueflow--insert-content "- item\n+ more\n")
    (should (string-match-p "^ +- item$" (buffer-string)))
    (should (string-match-p "^ +\\+ more$" (buffer-string)))
    (goto-char (point-min))
    (search-forward "item")
    (should (eq (get-text-property (1- (point)) 'face) 'trueflow-code-face))))
