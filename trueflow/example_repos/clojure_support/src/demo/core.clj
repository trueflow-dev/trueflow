(ns demo.core
  (:require [clojure.string :as str]
            [clojure.test :refer [deftest is testing]])
  (:import (java.time Instant)))

(def answer 42)

(defn normalize
  "Normalizes input values."
  [values]
  (let [trimmed (map str/trim values)]

    ;; keep only real values
    (->> trimmed
         (remove str/blank?)
         (map str/lower-case))))

(defmacro with-label [label & body]
  `(do
     (println ~label)
     ~@body))

(defmulti render :kind)

(defmethod render :text [{:keys [value]}]
  (str/trim value))

(defprotocol Renderable
  (render-item [this])
  (label [this prefix]))

(defrecord User [name]
  Renderable
  (render-item [this]
    name)

  (label [this prefix]
    (str prefix name)))

(deftype Counter [value]
  Object
  (toString [_]
    (str value)))

(require '[clojure.set :as set])
