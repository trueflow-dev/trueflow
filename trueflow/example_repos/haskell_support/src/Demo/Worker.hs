module Demo.Worker
  ( WorkerId(..)
  , Mode(..)
  , Renderable(..)
  , formatWorker
  ) where

import Data.List (intercalate)
import qualified Data.Text as Text

data Mode
  = Fast
  | Safe

newtype WorkerId = WorkerId Int

type Rendered = String

class Renderable a where
  render :: a -> Rendered
  render value = show value

instance Renderable WorkerId where
  render (WorkerId value) =
    show value

formatWorker :: WorkerId -> [Int] -> Rendered
formatWorker workerId values =
  let renderedWorker = render workerId

      renderedValues = intercalate "," (map show values)

      -- attach the worker prefix
      prefix = Text.unpack (Text.pack "worker")
  in prefix ++ ":" ++ renderedWorker ++ ":" ++ renderedValues
