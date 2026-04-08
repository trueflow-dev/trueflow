(ns demo.core-test
  (:require [clojure.test :refer [deftest is testing]]
            [demo.core :as core]))

(deftest normalize-test
  (testing "drops blanks and normalizes case"
    (is (= ["alpha" "beta"]
           (core/normalize [" Alpha " "" "BETA"])))))

(defn helper-value [value]
  value)
