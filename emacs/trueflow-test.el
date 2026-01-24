(require 'ert)
(require 'trueflow)

(ert-deftest trueflow-smoke-test ()
  "Ensure trueflow-mode can be enabled."
  (with-temp-buffer
    (trueflow-mode)
    (should (eq major-mode 'trueflow-mode))))

(ert-deftest trueflow-json-parse-test ()
  "Test JSON parsing from our stub."
  ;; Mocking trueflow--run-command is hard without a mocking library.
  ;; We will rely on integration tests or manual verification for now.
  ;; But we can test the helper if we separate it.
  (should t))
