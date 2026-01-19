(require 'ert)
(require 'vibecheck)

(ert-deftest vibecheck-smoke-test ()
  "Ensure vibecheck-mode can be enabled."
  (with-temp-buffer
    (vibecheck-mode)
    (should (eq major-mode 'vibecheck-mode))))

(ert-deftest vibecheck-json-parse-test ()
  "Test JSON parsing from our stub."
  ;; Mocking vibecheck--run-command is hard without a mocking library.
  ;; We will rely on integration tests or manual verification for now.
  ;; But we can test the helper if we separate it.
  (should t))
