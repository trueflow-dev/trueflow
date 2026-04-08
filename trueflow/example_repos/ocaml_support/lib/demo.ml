open Core
include Shared

module type WORKER = sig
  val run : int list -> int list
  type status =
    | Idle
    | Running of int
end

module Helpers = struct
  type config = {
    retries : int;
    mutable enabled : bool;
  }

  type outcome =
    | Ok of int
    | Error of string

  let version = 1

  module Inner = struct
    let helper value =
      value - 1
  end

  let rec normalize values =
    match values with
    | [] -> []
    | head :: tail ->
        let next = head + version in
        next :: normalize tail

  let build value =
    (* keep track of intermediate state *)

    let doubled = value * 2 in

    doubled + 1
end

type job = {
  id : int;
  name : string;
}

type mode =
  | Quick
  | Full of string

exception Invalid_job of string

external render : int -> string = "render_int"

let default_name = "worker"

let run values =
  let opened = List.map (fun value -> value + 1) values in

  opened
