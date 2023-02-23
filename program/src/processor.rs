use borsh::{BorshDeserialize, BorshSerialize};
use solana_program::{
    account_info::{next_account_info, AccountInfo},
    entrypoint::ProgramResult,
    msg,
    program::{invoke, invoke_signed},
    program_error::ProgramError,
    pubkey::Pubkey,
    system_instruction,
    sysvar::{rent::Rent, Sysvar},
};

use crate::{
    error::DataAccountError,
    instruction::DataAccountInstruction,
    state::{
        DataAccountMetadata, DataStatusOption, DataTypeOption, SerializationStatusOption,
        DATA_VERSION, METADATA_SIZE, PDA_SEED,
    },
};

pub struct Processor {}

impl Processor {
    pub fn process_instruction(
        program_id: &Pubkey,
        accounts: &[AccountInfo],
        instruction_data: &[u8],
    ) -> ProgramResult {
        let instruction = DataAccountInstruction::try_from_slice(instruction_data)
            .map_err(|_| ProgramError::InvalidInstructionData)?;

        match instruction {
            DataAccountInstruction::InitializeDataAccount(args) => {
                msg!("InitializeDataAccount");

                let accounts_iter = &mut accounts.iter();
                let authority = next_account_info(accounts_iter)?;
                let data_account = next_account_info(accounts_iter)?;
                let metadata_account = next_account_info(accounts_iter)?;
                let system_program = next_account_info(accounts_iter)?;

                // create a data_account of given space if not done so already
                if !args.is_created {
                    let space = args.space as usize;
                    let rent_exemption_amount = Rent::get()?.minimum_balance(space);

                    let create_account_ix = system_instruction::create_account(
                        &authority.key,
                        &data_account.key,
                        rent_exemption_amount,
                        space as u64,
                        &program_id,
                    );
                    invoke(
                        &create_account_ix,
                        &[
                            authority.clone(),
                            data_account.clone(),
                            system_program.clone(),
                        ],
                    )?;
                }
                data_account.data.borrow_mut().fill(0);

                // create data_account pda to store metadata
                let (pda, bump_seed) = Pubkey::find_program_address(
                    &[PDA_SEED, data_account.key.as_ref()],
                    program_id,
                );
                // ensure the pda is valid
                if pda != *metadata_account.key {
                    return Err(DataAccountError::InvalidPDA.into());
                }
                // create pda account
                let rent_exemption_amount = Rent::get()?.minimum_balance(METADATA_SIZE);
                let create_pda_ix = system_instruction::create_account(
                    &authority.key,
                    &metadata_account.key,
                    rent_exemption_amount,
                    METADATA_SIZE as u64,
                    &program_id,
                );
                invoke_signed(
                    &create_pda_ix,
                    &[
                        authority.clone(),
                        data_account.clone(),
                        metadata_account.clone(),
                        system_program.clone(),
                    ],
                    &[&[PDA_SEED, data_account.key.as_ref(), &[bump_seed]]],
                )?;
                // create initial state for data_account metadata and write to it
                let account_metadata = DataAccountMetadata::new(
                    DataStatusOption::INITIALIZED,
                    SerializationStatusOption::UNVERIFIED,
                    args.authority,
                    args.is_dynamic,
                    DATA_VERSION,
                    DataTypeOption::CUSTOM,
                    bump_seed,
                );
                account_metadata.serialize(&mut &mut metadata_account.data.borrow_mut()[..])?;

                Ok(())
            }
            DataAccountInstruction::UpdateDataAccount(args) => {
                msg!("UpdateDataAccount");

                let accounts_iter = &mut accounts.iter();
                let authority = next_account_info(accounts_iter)?;
                let data_account = next_account_info(accounts_iter)?;
                let metadata_account = next_account_info(accounts_iter)?;
                let system_program = next_account_info(accounts_iter)?;

                // ensure authority and data_account are signer
                if !authority.is_signer || !data_account.is_signer {
                    return Err(DataAccountError::NotSigner.into());
                }

                // ensure authority, data_account, and metadata_account are writable
                if !authority.is_writable
                    || !data_account.is_writable
                    || !metadata_account.is_writable
                {
                    return Err(DataAccountError::NotWriteable.into());
                }

                // ensure length is not 0
                if metadata_account.data_is_empty() {
                    return Err(DataAccountError::NoAccountLength.into());
                }

                let mut account_metadata =
                    DataAccountMetadata::try_from_slice(&metadata_account.try_borrow_data()?)?;

                // ensure data_account is initialized
                if *account_metadata.data_status() == DataStatusOption::UNINITIALIZED {
                    return Err(DataAccountError::NotInitialized.into());
                }

                // ensure data_account is being written to by valid authority
                if account_metadata.authority() != authority.key {
                    return Err(DataAccountError::InvalidAuthority.into());
                }

                // ensure the metadata_account corresponds to the data_account
                let pda = Pubkey::create_program_address(
                    &[
                        PDA_SEED,
                        data_account.key.as_ref(),
                        &[account_metadata.bump_seed()],
                    ],
                    program_id,
                )?;
                if pda != *metadata_account.key {
                    return Err(DataAccountError::InvalidPDA.into());
                }

                let old_len = data_account.data_len();
                let end_len = args.offset as usize + args.data.len();

                // ensure static data_account has sufficient space
                if !account_metadata.dynamic() && old_len < end_len {
                    return Err(DataAccountError::InsufficientSpace.into());
                }

                let new_len = if !account_metadata.dynamic() {
                    old_len
                } else if args.realloc_down {
                    end_len
                } else {
                    old_len.max(end_len)
                };

                // update the metadata_account
                account_metadata.set_data_type(args.data_type);
                account_metadata.serialize(&mut &mut metadata_account.data.borrow_mut()[..])?;

                // ensure data_account has enough space by reallocing if needed
                if old_len != new_len {
                    let new_space = new_len;
                    let new_minimum_balance = Rent::get()?.minimum_balance(new_space);
                    let lamports_diff = if old_len < new_len {
                        new_minimum_balance.saturating_sub(data_account.lamports())
                    } else {
                        data_account.lamports().saturating_sub(new_minimum_balance)
                    };

                    if old_len < new_len {
                        let transfer_ix = system_instruction::transfer(
                            authority.key,
                            data_account.key,
                            lamports_diff,
                        );
                        invoke(
                            &transfer_ix,
                            &[
                                authority.clone(),
                                data_account.clone(),
                                system_program.clone(),
                            ],
                        )?;
                    } else {
                        let authority_lamports = authority.lamports();
                        **authority.lamports.borrow_mut() = authority_lamports
                            .checked_add(lamports_diff)
                            .ok_or(DataAccountError::Overflow)?;
                        **data_account.lamports.borrow_mut() = new_minimum_balance;
                    }

                    data_account.realloc(new_space, false)?;
                }

                // update the data_account
                data_account.data.borrow_mut()[args.offset as usize..end_len]
                    .copy_from_slice(&args.data);

                Ok(())
            }
            DataAccountInstruction::CloseDataAccount(_args) => {
                msg!("CloseDataAccount");

                let accounts_iter = &mut accounts.iter();
                let authority = next_account_info(accounts_iter)?;
                let data_account = next_account_info(accounts_iter)?;
                let metadata_account = next_account_info(accounts_iter)?;

                // ensure authority is signer
                if !authority.is_signer {
                    return Err(DataAccountError::NotSigner.into());
                }

                // ensure authority, data_account, and metadata_account are writable
                if !authority.is_writable
                    || !data_account.is_writable
                    || !metadata_account.is_writable
                {
                    return Err(DataAccountError::NotWriteable.into());
                }

                // ensure length is not 0
                if data_account.data_is_empty() || metadata_account.data_is_empty() {
                    return Err(DataAccountError::NoAccountLength.into());
                }

                let account_metadata =
                    DataAccountMetadata::try_from_slice(&metadata_account.try_borrow_data()?)?;

                // ensure data_account is initialized
                if *account_metadata.data_status() == DataStatusOption::UNINITIALIZED {
                    return Err(DataAccountError::NotInitialized.into());
                }

                // ensure data_account is being closed by valid authority
                if account_metadata.authority() != authority.key {
                    return Err(DataAccountError::InvalidAuthority.into());
                }

                // ensure the metadata_account corresponds to the data_account
                let pda = Pubkey::create_program_address(
                    &[
                        PDA_SEED,
                        data_account.key.as_ref(),
                        &[account_metadata.bump_seed()],
                    ],
                    program_id,
                )?;
                if pda != *metadata_account.key {
                    return Err(DataAccountError::InvalidPDA.into());
                }

                // transfer metadata_account lamports back to authority and reset metadata_account
                let curr_lamports = authority.lamports();
                **authority.lamports.borrow_mut() = curr_lamports
                    .checked_add(metadata_account.lamports())
                    .ok_or(DataAccountError::Overflow)?;
                **metadata_account.lamports.borrow_mut() = 0;
                metadata_account.data.borrow_mut().fill(0);

                // transfer data_account lamports back to authority and reset data_account
                let curr_lamports = authority.lamports();
                **authority.lamports.borrow_mut() = curr_lamports
                    .checked_add(data_account.lamports())
                    .ok_or(DataAccountError::Overflow)?;
                **data_account.lamports.borrow_mut() = 0;
                data_account.data.borrow_mut().fill(0);

                Ok(())
            }
        }
    }
}
