# Hyperspace Light

## Overview

Hyperspace is an off-chain relayer component of ComposableFi’s [Centauri]((https://github.com/ComposableFi/centauri)) bridge protocol, aiming to relay IBC datagrams between any IBC-enabled chains.</br>
This repository contains a light version of Hyperspace (Parachain-related components extracted out) to relay IBC datagrams between Cosmos chains. </br>
This work strived to investigate the feasibility of relaying between two Gaia chains using Hyperspace architecture.

## Architecture

[Here](https://excalidraw.com/#json=vHMej5cS0xjZsP3yr1GhU,PJlqjjoQ5BkHpjEHnGz4Fg) is some diagrams that visualizes various elements of the relayer and represents their interactions.

## Run Test

To have a test packet relayed, first, you have to run two local gaia chains, following steps [3.1](https://hermes.informal.systems/tutorials/pre-requisites/index.html) and [3.2](https://hermes.informal.systems/tutorials/local-chains/index.html). Then execute the test by running below command:</br>
  
  ```bash
  cargo test -p hyperspace-light --test cosmos_cosmos
  ```
