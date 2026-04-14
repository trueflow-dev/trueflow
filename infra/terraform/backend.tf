terraform {
  backend "s3" {
    bucket  = "jm-deploy-state-bucket"
    key     = "trueflow/site/terraform.tfstate"
    region  = "us-west-2"
    encrypt = true
  }
}
