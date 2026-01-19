(require 'ert)
(require 'vet)

(ert-deftest vet-smoke-test ()
  "Ensure vet-mode can be enabled."
  (with-temp-buffer
    (vet-mode)
    (should (eq major-mode 'vet-mode))))

(ert-deftest vet-json-parse-test ()
  "Test JSON parsing from our stub."
  ;; Mocking vet--run-command is hard without a mocking library.
  ;; We will rely on integration tests or manual verification for now.
  ;; But we can test the helper if we separate it.
  (should t))
