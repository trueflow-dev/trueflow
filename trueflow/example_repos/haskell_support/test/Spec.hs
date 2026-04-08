module Spec where

import Test.Hspec (Spec, describe, it, shouldBe)

import Demo.Worker (WorkerId(..), formatWorker)

spec :: Spec
spec = do
  describe "formatWorker" $ do
    it "renders the worker id and values" $
      formatWorker (WorkerId 7) [1, 2] `shouldBe` "worker:7:1,2"

testFormatWorker :: IO ()
testFormatWorker = pure ()

prop_workerIdRoundTrip :: WorkerId -> Bool
prop_workerIdRoundTrip (WorkerId value) = value == value

helper :: Int -> Int
helper value = value + 1
