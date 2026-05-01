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
    (start_line . 0)
    (end_line . 0)
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
  "Run ACTION-FN and return (OPENED REVIEW-INDEX REVIEW-HASHES)."
  (let* ((initial-hashes '("a" "b" "c"))
         (remaining-hashes '("b" "c"))
         (initial-scan (trueflow-test--scan-data initial-hashes))
         (remaining-scan (trueflow-test--scan-data remaining-hashes))
         (status-buf (generate-new-buffer " *trueflow-test-status*"))
         (focus-buf (generate-new-buffer " *trueflow-test-focus*"))
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
                    ((symbol-function 'trueflow--run-mark) (lambda (&rest _) t))
                    ((symbol-function 'trueflow-refresh)
                     (lambda ()
                       (setq trueflow-last-scan-data remaining-scan)))
                    ((symbol-function 'trueflow--fetch-subblocks) (lambda (&rest _) nil))
                    ((symbol-function 'trueflow-focus-open)
                     (lambda (block _status-buf current-idx total-count)
                       (setq opened (list (alist-get 'hash block) current-idx total-count)))))
            (with-current-buffer focus-buf
              (funcall action-fn)))
          (list opened
                (with-current-buffer status-buf trueflow-review-index)
                (with-current-buffer status-buf
                  (mapcar (lambda (block) (alist-get 'hash block))
                          trueflow-review-items))))
      (kill-buffer status-buf)
      (kill-buffer focus-buf))))

(ert-deftest trueflow-smoke-test ()
  "Ensure trueflow-mode can be enabled."
  (with-temp-buffer
    (trueflow-mode)
    (should (eq major-mode 'trueflow-mode))))

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

(ert-deftest trueflow-review-header-lines-prefer_path_then_named_block ()
  "Review headers should use a compact path, named block line, and subblock tree."
  (let ((block '((hash . "a85ccbc912345678")
                 (content . "#[derive(Debug, Clone)]\nstruct Config {\n    name: String,\n}\n")
                 (start_line . 0)
                 (end_line . 3)
                 (kind . "struct")
                 (file . "./example_repos/all_languages/main.rs")))
        (subblocks '(((kind . "CodeParagraph")))))
    (cl-letf (((symbol-function 'trueflow-root) (lambda (&optional _) "/repo/")))
      (should (equal (trueflow--review-header-lines block subblocks)
                     '("example_repos/all_languages/main.rs"
                       "  -> struct Config (hash=a85ccbc9)"
                       "     └─ CodeParagraph"))))))

(ert-deftest trueflow-focus-approve-refreshes-without-skipping-next-item ()
  "Approve should advance to the next refreshed item, not skip it."
  (pcase-let ((`(,opened ,review-index ,review-hashes)
                (trueflow-test--exercise-focus-action #'trueflow-focus-approve)))
    (should (equal opened '("b" 1 2)))
    (should (= review-index 0))
    (should (equal review-hashes '("b" "c")))))

(ert-deftest trueflow-focus-reject-refreshes-without-skipping-next-item ()
  "Reject should advance to the next refreshed item, not skip it."
  (pcase-let ((`(,opened ,review-index ,review-hashes)
                (trueflow-test--exercise-focus-action #'trueflow-focus-reject)))
    (should (equal opened '("b" 1 2)))
    (should (= review-index 0))
    (should (equal review-hashes '("b" "c")))))

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
