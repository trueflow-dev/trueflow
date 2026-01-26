(require 'cl-lib)

(defconst all-languages-max-retries 3)
(defvar all-languages-default-name "sample")

(cl-defstruct (all-languages-config (:constructor all-languages-config-create))
  name
  threshold)

(cl-defgeneric all-languages-process (processor values))

(cl-defstruct (all-languages-multiplier (:constructor all-languages-multiplier-create))
  factor)

(cl-defmethod all-languages-process ((processor all-languages-multiplier) values)
  (let ((factor (all-languages-multiplier-factor processor)))
    (mapcar (lambda (value) (* value factor)) values)))

(defun all-languages-collect-until (limit)
  (let ((values nil)
        (current 0))
    (while (< current limit)
      (push current values)
      (setq current (1+ current)))
    (nreverse values)))

(defun all-languages-main ()
  (let* ((config (all-languages-config-create
                  :name all-languages-default-name
                  :threshold 4))
         (processor (all-languages-multiplier-create :factor 2))
         (values (all-languages-collect-until
                  (all-languages-config-threshold config)))
         (processed (all-languages-process processor values)))
    (dotimes (attempt all-languages-max-retries)
      (message "attempt %d" attempt))
    (message "%s: %S" (all-languages-config-name config) processed)))

(all-languages-main)
