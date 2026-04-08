open Core
include Shared

module type WORKER = sig
  val run : int list -> int list
  type status =
    | Idle
    | Running of int
end

module Helpers : sig
  type config = {
    retries : int;
    enabled : bool;
  }

  module Inner : sig
    val helper : int -> int
  end

  val build : int -> int
end

type mode =
  | Quick
  | Full of string

val render : int -> string
val default_name : string
