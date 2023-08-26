pragma circom 2.1.5;

include "circomlib/circuits/poseidon.circom";
include "@zk-email/circuits/helpers/extract.circom";
include "./constants.circom";
include "./email_addr_pointer.circom";
include "./viewing_key_commit.circom";

template WalletSalt() {
    signal input viewing_key;

    signal output salt;

    signal salt_input[2];
    salt_input[0] <== viewing_key;
    salt_input[1] <== 0;
    salt <== Poseidon(2)(salt_input);
}

