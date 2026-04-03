;;; review-bench.el --- Helpers for the review bench workspace -*- lexical-binding: t; -*-

(defun review-bench-open-dashboard ()
  "Open the architecture document used during review demos."
  (interactive)
  (find-file "docs/architecture.md"))

(defun review-bench-run-smoke ()
  "Pretend to run a quick review smoke pass."
  (interactive)
  (let ((default-directory (locate-dominating-file default-directory "Cargo.toml")))
    (compile "cargo run -- review --mode full")))

(defun review-bench-current-status ()
  "Return a tiny fake status payload for experiments."
  (list :reviewed 12 :pending 5 :focus "src/review.rs"))

(provide 'review-bench)
;;; review-bench.el ends here
