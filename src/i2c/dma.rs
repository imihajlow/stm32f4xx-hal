use core::{marker::PhantomData, mem::transmute};

use super::{I2c, Instance};
use crate::dma::{
    config::DmaConfig,
    traits::{Channel, DMASet, PeriAddress, Stream},
    ChannelX, MemoryToPeripheral, PeripheralToMemory, Transfer,
};

use nb;

#[non_exhaustive]
pub enum Error {
    I2CError(super::Error),
    TransferError,
}

/// Callback type to notify user code of completion I2C transfers
pub type I2cCompleteCallback = fn(Result<(), Error>);

pub trait I2CMasterWriteDMA {
    /// Writes `bytes` to slave with address `addr` in non-blocking mode
    ///
    /// # Arguments
    /// * `addr` - slave address
    /// * `bytes` - byte slice that need to send
    /// * `callback` - callback that will be called on completion
    ///
    /// # Safety
    /// This function relies on supplied slice `bytes` until `callback` called. So the slice must live until that moment.
    ///
    /// # Warning
    /// `callback` may be called before function returns value. It happens on errors in preparation stages.
    unsafe fn write_dma(
        &mut self,
        addr: u8,
        bytes: &[u8],
        callback: Option<I2cCompleteCallback>,
    ) -> nb::Result<(), super::Error>;
}

pub trait I2CMasterReadDMA {
    /// Reads bytes from slave device with address `addr` in non-blocking mode and writes these bytes in `buf`
    ///
    /// # Arguments
    /// * `addr` - slave address
    /// * `buf` - byte slice where received bytes will be written
    /// * `callback` - callback that will be called on completion
    ///
    /// # Safety
    /// This function relies on supplied slice `buf` until `callback` called. So the slice must live until that moment.
    ///
    /// # Warning
    /// `callback` may be called before function returns value. It happens on errors in preparation stages.
    unsafe fn read_dma(
        &mut self,
        addr: u8,
        buf: &mut [u8],
        callback: Option<I2cCompleteCallback>,
    ) -> nb::Result<(), super::Error>;
}

pub trait I2CMasterWriteReadDMA {
    /// Writes `bytes` to slave with address `addr` in non-blocking mode and then generate ReStart and receive a bytes from a same device
    ///
    /// # Arguments
    /// * `addr` - slave address
    /// * `bytes` - byte slice that need to send
    /// * `buf` - byte slice where received bytes will be written
    /// * `callback` - callback that will be called on completion
    ///
    /// # Safety
    /// This function relies on supplied slices `bytes` and `buf` until `callback` called. So slices must live until that moment.
    ///
    /// # Warning
    /// `callback` may be called before function returns value. It happens on errors in preparation stages.
    unsafe fn write_read_dma(
        &mut self,
        addr: u8,
        bytes: &[u8],
        buf: &mut [u8],
        callback: Option<I2cCompleteCallback>,
    ) -> nb::Result<(), super::Error>;
}

impl<I2C: Instance, PINS> I2c<I2C, PINS> {
    /// Converts blocking [I2c] to non-blocking [I2CMasterDma] that use `tx_stream` and `rx_stream` to send/receive data
    pub fn use_dma<TX_STREAM, const TX_CH: u8, RX_STREAM, const RX_CH: u8>(
        self,
        tx_stream: TX_STREAM,
        rx_stream: RX_STREAM,
    ) -> I2CMasterDma<I2C, PINS, TX_STREAM, TX_CH, RX_STREAM, RX_CH>
    where
        TX_STREAM: Stream,
        RX_STREAM: Stream,
    {
        let tx = Tx { i2c: PhantomData };
        let rx = Rx { i2c: PhantomData };

        I2CMasterDma {
            hal_i2c: self,
            callback: None,

            address: 0,
            rx_len: 0,

            tx: Some(tx),
            tx_stream: Some(tx_stream),
            tx_transfer: None,

            rx: Some(rx),
            rx_stream: Some(rx_stream),
            rx_transfer: None,
        }
    }
}

/// I2c abstraction that can work in non-blocking mode by using DMA
///
/// The struct should be used for sending/receiving bytes to/from slave device in non-blocking mode.
/// A client must follow these requirements to use that feature:
/// * Enable interrupts DMAx_STREAMy used for transmit and another DMAq_STREAMp used for receive.
/// * In these interrupts call [`handle_dma_interrupt`](Self::handle_dma_interrupt)
/// * Enable interrupts I2Cx_ER for handling errors and call [`handle_error_interrupt`](Self::handle_error_interrupt) in corresponding handler
///
/// The struct can be also used to send/receive bytes in blocking mode with methods:
/// [`write`](Self::write()), [`read`](Self::read()), [`write_read`](Self::write_read()).
///
pub struct I2CMasterDma<I2C, PINS, TX_STREAM, const TX_CH: u8, RX_STREAM, const RX_CH: u8>
where
    I2C: Instance,
    TX_STREAM: Stream,
    RX_STREAM: Stream,
{
    hal_i2c: I2c<I2C, PINS>,

    callback: Option<I2cCompleteCallback>,

    /// Last address used in `write_read_dma` method
    address: u8,
    /// Len of `buf` in `write_read_dma` method
    rx_len: usize,

    tx: Option<Tx<I2C>>,
    tx_stream: Option<TX_STREAM>,
    tx_transfer: Option<Transfer<TX_STREAM, TX_CH, Tx<I2C>, MemoryToPeripheral, &'static [u8]>>,

    rx: Option<Rx<I2C>>,
    rx_stream: Option<RX_STREAM>,
    rx_transfer: Option<Transfer<RX_STREAM, RX_CH, Rx<I2C>, PeripheralToMemory, &'static mut [u8]>>,
}

impl<I2C, PINS, TX_STREAM, const TX_CH: u8, RX_STREAM, const RX_CH: u8>
    I2CMasterDma<I2C, PINS, TX_STREAM, TX_CH, RX_STREAM, RX_CH>
where
    I2C: Instance,
    TX_STREAM: Stream,
    ChannelX<TX_CH>: Channel,
    Tx<I2C>: DMASet<TX_STREAM, TX_CH, MemoryToPeripheral>,

    RX_STREAM: Stream,
    ChannelX<RX_CH>: Channel,
    Rx<I2C>: DMASet<RX_STREAM, RX_CH, PeripheralToMemory>,
{
    fn call_callback_once(&mut self, res: Result<(), Error>) {
        if let Some(c) = self.callback.take() {
            c(res);
        }
    }

    fn create_tx_transfer(&mut self, buf: &'static [u8]) {
        assert!(self.tx.is_some());
        assert!(self.tx_stream.is_some());

        let transfer = Transfer::init_memory_to_peripheral(
            self.tx_stream.take().unwrap(),
            self.tx.take().unwrap(),
            buf,
            None,
            DmaConfig::default()
                .memory_increment(true)
                .transfer_complete_interrupt(true)
                .transfer_error_interrupt(true),
        );

        self.tx_transfer = Some(transfer);
    }

    fn destroy_tx_transfer(&mut self) {
        assert!(self.tx_transfer.is_some());

        let (str, tx, ..) = self.tx_transfer.take().unwrap().release();
        self.tx = Some(tx);
        self.tx_stream = Some(str);
    }

    fn create_rx_transfer(&mut self, buf: &'static mut [u8]) {
        assert!(self.rx.is_some());
        assert!(self.rx_stream.is_some());

        let transfer = Transfer::init_peripheral_to_memory(
            self.rx_stream.take().unwrap(),
            self.rx.take().unwrap(),
            buf,
            None,
            DmaConfig::default()
                .memory_increment(true)
                .transfer_complete_interrupt(true)
                .transfer_error_interrupt(true),
        );

        self.rx_transfer = Some(transfer);
    }

    fn destroy_rx_transfer(&mut self) {
        assert!(self.rx_transfer.is_some());

        let (str, tx, ..) = self.rx_transfer.take().unwrap().release();
        self.rx = Some(tx);
        self.rx_stream = Some(str);
    }

    /// Checks if there is communication in progress
    #[inline(always)]
    pub fn busy(&self) -> bool {
        self.hal_i2c.i2c.sr2.read().busy().bit_is_set()
    }

    /// Like `busy` but returns `WouldBlock` if busy
    fn busy_res(&self) -> nb::Result<(), super::Error> {
        if self.busy() {
            return nb::Result::Err(nb::Error::WouldBlock);
        }
        Ok(())
    }

    #[inline(always)]
    fn enable_dma_requests(&mut self) {
        self.hal_i2c.i2c.cr2.modify(|_, w| w.dmaen().enabled());
    }

    #[inline(always)]
    fn disable_dma_requests(&mut self) {
        self.hal_i2c.i2c.cr2.modify(|_, w| w.dmaen().disabled());
    }

    #[inline(always)]
    fn enable_error_interrupt_generation(&mut self) {
        self.hal_i2c.i2c.cr2.modify(|_, w| w.iterren().enabled());
    }

    #[inline(always)]
    fn disable_error_interrupt_generation(&mut self) {
        self.hal_i2c.i2c.cr2.modify(|_, w| w.iterren().disabled());
    }

    fn send_start(&mut self, read: bool) -> Result<(), super::Error> {
        let i2c = &self.hal_i2c.i2c;
        i2c.cr1.modify(|_, w| w.start().set_bit());
        if read {
            i2c.cr1.modify(|_, w| w.ack().set_bit());
        }

        // Wait until START condition was generated
        while self
            .hal_i2c
            .check_and_clear_error_flags()?
            .sb()
            .bit_is_clear()
        {}

        // Also wait until signalled we're master and everything is waiting for us
        loop {
            self.hal_i2c.check_and_clear_error_flags()?;

            let sr2 = i2c.sr2.read();
            if !(sr2.msl().bit_is_clear() && sr2.busy().bit_is_clear()) {
                break;
            }
        }

        Ok(())
    }

    fn send_stop(&mut self) {
        self.hal_i2c.i2c.cr1.modify(|_, w| w.stop().set_bit());
    }

    fn send_address(&mut self, addr: u8, read: bool) -> Result<(), super::Error> {
        let i2c = &self.hal_i2c.i2c;

        let mut to_send_addr = u32::from(addr) << 1;
        if read {
            to_send_addr += 1;
        }

        // Set up current address, we're trying to talk to
        i2c.dr.write(|w| unsafe { w.bits(to_send_addr) });

        // Wait until address was sent
        loop {
            // Check for any I2C errors. If a NACK occurs, the ADDR bit will never be set.
            let sr1 = self
                .hal_i2c
                .check_and_clear_error_flags()
                .map_err(super::Error::nack_addr)?;

            // Wait for the address to be acknowledged
            if sr1.addr().bit_is_set() {
                break;
            }
        }

        Ok(())
    }

    fn prepare_write(&mut self, addr: u8) -> Result<(), super::Error> {
        // Start
        self.send_start(false)?;

        // Send address
        self.send_address(addr, false)?;

        // Clear condition by reading SR2. This will clear ADDR flag
        self.hal_i2c.i2c.sr2.read();

        // Enable error interrups
        self.enable_error_interrupt_generation();

        Ok(())
    }

    /// Generates start and send addres for read commands
    fn prepare_read(&mut self, addr: u8, buf_len: usize) -> Result<(), super::Error> {
        // Start
        self.send_start(true)?;

        // Send address
        self.send_address(addr, true)?;

        // On small sized array we need to set ACK=0 before ADDR cleared
        if buf_len <= 1 {
            self.hal_i2c.i2c.cr1.modify(|_, w| w.ack().clear_bit());
        }

        // Clear condition by reading SR2. This will clear ADDR flag
        self.hal_i2c.i2c.sr2.read();

        // Enable error interrups
        self.enable_error_interrupt_generation();

        Ok(())
    }

    fn finish_transfer_with_result(&mut self, result: Result<(), Error>) {
        self.disable_dma_requests();
        self.disable_error_interrupt_generation();

        if let Err(Error::I2CError(super::Error::NoAcknowledge(_))) = &result {
            self.send_stop();
        }

        self.call_callback_once(result);

        if self.tx_transfer.is_some() {
            self.destroy_tx_transfer();
        }

        if self.rx_transfer.is_some() {
            self.destroy_rx_transfer();
        }
    }

    /// Handles DMA interrupt.
    /// This method a client must call in DMAx_STREAMy interrupt
    pub fn handle_dma_interrupt(&mut self) {
        // Handle Transmit
        if let Some(tx_t) = &mut self.tx_transfer {
            if TX_STREAM::get_fifo_error_flag() {
                tx_t.clear_fifo_error_interrupt();

                return;
            }

            if TX_STREAM::get_transfer_error_flag() {
                tx_t.clear_transfer_error_interrupt();

                self.finish_transfer_with_result(Err(Error::TransferError));

                return;
            }

            if TX_STREAM::get_transfer_complete_flag() {
                tx_t.clear_transfer_complete_interrupt();

                // If we have prepared Rx Transfer, there are write_read command, generate restart signal and do not disable DMA requests
                // Indicate that we have read after this transmit
                let have_read_after = self.rx_transfer.is_some();

                self.destroy_tx_transfer();
                if !have_read_after {
                    self.finish_transfer_with_result(Ok(()));
                }

                // Wait for BTF
                while self.hal_i2c.i2c.sr1.read().btf().bit_is_clear() {}

                // If we have prepared Rx Transfer, there are write_read command, generate restart signal
                if have_read_after {
                    // Prepare for reading
                    if let Err(e) = self.prepare_read(self.address, self.rx_len) {
                        self.finish_transfer_with_result(Err(Error::I2CError(e)))
                    }

                    self.rx_transfer.as_mut().unwrap().start(|_| {});
                } else {
                    // Generate stop and wait for it
                    self.send_stop();
                }

                return;
            }

            // If Transmit handled then receive should not be handled even if exists.
            // This return protects for handling Tx and Rx events in one interrupt.
            return;
        }

        if let Some(rx_t) = &mut self.rx_transfer {
            if RX_STREAM::get_fifo_error_flag() {
                rx_t.clear_fifo_error_interrupt();

                return;
            }

            if RX_STREAM::get_transfer_error_flag() {
                rx_t.clear_transfer_error_interrupt();

                self.finish_transfer_with_result(Err(Error::TransferError));

                return;
            }

            if RX_STREAM::get_transfer_complete_flag() {
                rx_t.clear_transfer_complete_interrupt();

                self.finish_transfer_with_result(Ok(()));

                // Clear ACK
                self.hal_i2c.i2c.cr1.modify(|_, w| w.ack().clear_bit());
                // Generate stop and wait for it
                self.send_stop();
            }
        }
    }

    /// Handles errors generated by I2C
    /// This method a client must call in I2Cx_ER interrupt
    pub fn handle_error_interrupt(&mut self) {
        let res = self.hal_i2c.check_and_clear_error_flags();
        if let Err(e) = res {
            self.finish_transfer_with_result(Err(Error::I2CError(e)));
        }
    }

    /// Reads in blocking mode but if i2c is busy returns `WouldBlock` and do nothing
    pub fn read(&mut self, addr: u8, buffer: &mut [u8]) -> nb::Result<(), super::Error> {
        self.busy_res()?;
        match self.hal_i2c.read(addr, buffer) {
            Ok(_) => Ok(()),
            Err(super::Error::NoAcknowledge(source)) => {
                self.send_stop();
                Err(nb::Error::Other(super::Error::NoAcknowledge(source)))
            }
            Err(error) => Err(nb::Error::Other(error)),
        }
    }

    /// Write and then read in blocking mode but if i2c is busy returns `WouldBlock` and do nothing
    pub fn write_read(
        &mut self,
        addr: u8,
        bytes: &[u8],
        buffer: &mut [u8],
    ) -> nb::Result<(), super::Error> {
        self.busy_res()?;
        match self.hal_i2c.write_read(addr, bytes, buffer) {
            Ok(_) => Ok(()),
            Err(super::Error::NoAcknowledge(source)) => {
                self.send_stop();
                Err(nb::Error::Other(super::Error::NoAcknowledge(source)))
            }
            Err(error) => Err(nb::Error::Other(error)),
        }
    }

    /// Write in blocking mode but if i2c is busy returns `WouldBlock` and do nothing
    pub fn write(&mut self, addr: u8, bytes: &[u8]) -> nb::Result<(), super::Error> {
        self.busy_res()?;
        match self.hal_i2c.write(addr, bytes) {
            Ok(_) => Ok(()),
            Err(super::Error::NoAcknowledge(source)) => {
                self.send_stop();
                Err(nb::Error::Other(super::Error::NoAcknowledge(source)))
            }
            Err(error) => Err(nb::Error::Other(error)),
        }
    }
}

impl<I2C, PINS, TX_STREAM, const TX_CH: u8, RX_STREAM, const RX_CH: u8> I2CMasterWriteDMA
    for I2CMasterDma<I2C, PINS, TX_STREAM, TX_CH, RX_STREAM, RX_CH>
where
    I2C: Instance,
    TX_STREAM: Stream,
    ChannelX<TX_CH>: Channel,
    Tx<I2C>: DMASet<TX_STREAM, TX_CH, MemoryToPeripheral>,

    RX_STREAM: Stream,
    ChannelX<RX_CH>: Channel,
    Rx<I2C>: DMASet<RX_STREAM, RX_CH, PeripheralToMemory>,
{
    unsafe fn write_dma(
        &mut self,
        addr: u8,
        bytes: &[u8],
        callback: Option<I2cCompleteCallback>,
    ) -> nb::Result<(), super::Error> {
        self.busy_res()?;

        // Prepare transfer
        self.enable_dma_requests();
        let static_bytes: &'static [u8] = transmute(bytes);
        self.create_tx_transfer(static_bytes);
        self.callback = callback;

        if let Err(e) = self.prepare_write(addr) {
            // Reset struct on errors
            self.finish_transfer_with_result(Err(Error::I2CError(e)));
            return Err(nb::Error::Other(e));
        }

        // Start DMA processing
        self.tx_transfer.as_mut().unwrap().start(|_| {});

        Ok(())
    }
}

impl<I2C, PINS, TX_STREAM, const TX_CH: u8, RX_STREAM, const RX_CH: u8> I2CMasterReadDMA
    for I2CMasterDma<I2C, PINS, TX_STREAM, TX_CH, RX_STREAM, RX_CH>
where
    I2C: Instance,
    TX_STREAM: Stream,
    ChannelX<TX_CH>: Channel,
    Tx<I2C>: DMASet<TX_STREAM, TX_CH, MemoryToPeripheral>,

    RX_STREAM: Stream,
    ChannelX<RX_CH>: Channel,
    Rx<I2C>: DMASet<RX_STREAM, RX_CH, PeripheralToMemory>,
{
    unsafe fn read_dma(
        &mut self,
        addr: u8,
        buf: &mut [u8],
        callback: Option<I2cCompleteCallback>,
    ) -> nb::Result<(), super::Error> {
        self.busy_res()?;

        //  If size is small we need to set ACK=0 before cleaning ADDR(reading SR2)
        let buf_len = buf.len();

        self.enable_dma_requests();
        let static_buf: &'static mut [u8] = transmute(buf);
        self.create_rx_transfer(static_buf);
        self.callback = callback;

        if let Err(e) = self.prepare_read(addr, buf_len) {
            // Reset struct on errors
            self.finish_transfer_with_result(Err(Error::I2CError(e)));
            return Err(nb::Error::Other(e));
        }

        // Start DMA processing
        self.rx_transfer.as_mut().unwrap().start(|_| {});

        Ok(())
    }
}

impl<I2C, PINS, TX_STREAM, const TX_CH: u8, RX_STREAM, const RX_CH: u8> I2CMasterWriteReadDMA
    for I2CMasterDma<I2C, PINS, TX_STREAM, TX_CH, RX_STREAM, RX_CH>
where
    I2C: Instance,
    TX_STREAM: Stream,
    ChannelX<TX_CH>: Channel,
    Tx<I2C>: DMASet<TX_STREAM, TX_CH, MemoryToPeripheral>,

    RX_STREAM: Stream,
    ChannelX<RX_CH>: Channel,
    Rx<I2C>: DMASet<RX_STREAM, RX_CH, PeripheralToMemory>,
{
    unsafe fn write_read_dma(
        &mut self,
        addr: u8,
        bytes: &[u8],
        buf: &mut [u8],
        callback: Option<I2cCompleteCallback>,
    ) -> nb::Result<(), super::Error> {
        self.busy_res()?;

        self.address = addr;
        self.rx_len = buf.len();

        self.enable_dma_requests();
        let static_bytes: &'static [u8] = transmute(bytes);
        self.create_tx_transfer(static_bytes);
        let static_buf: &'static mut [u8] = transmute(buf);
        self.create_rx_transfer(static_buf);
        self.callback = callback;

        if let Err(e) = self.prepare_write(addr) {
            // Reset struct on errors
            self.finish_transfer_with_result(Err(Error::I2CError(e)));
            return Err(nb::Error::Other(e));
        }

        // Start DMA processing
        self.tx_transfer.as_mut().unwrap().start(|_| {});

        Ok(())
    }
}

pub struct Tx<I2C> {
    i2c: PhantomData<I2C>,
}

pub struct Rx<I2C> {
    i2c: PhantomData<I2C>,
}

unsafe impl<I2C: Instance> PeriAddress for Rx<I2C> {
    #[inline(always)]
    fn address(&self) -> u32 {
        unsafe { &(*I2C::ptr()).dr as *const _ as u32 }
    }

    type MemSize = u8;
}

unsafe impl<I2C: Instance> PeriAddress for Tx<I2C> {
    #[inline(always)]
    fn address(&self) -> u32 {
        unsafe { &(*I2C::ptr()).dr as *const _ as u32 }
    }

    type MemSize = u8;
}
