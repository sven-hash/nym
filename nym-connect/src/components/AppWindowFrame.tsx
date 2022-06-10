import React from 'react';
import { Box } from '@mui/material';
import { invoke } from '@tauri-apps/api/tauri';

export const AppWindowFrame: React.FC = ({ children }) => (
  <Box
    sx={{
      background: '#121726',
      borderRadius: '12px',
      padding: '12px 16px',
      display: 'grid',
      gridTemplateRows: '30px auto',
      width: '240px',
    }}
  >
    <Box display="flex" justifyContent="space-between" alignItems="center">
      <svg width="22" height="6" viewBox="0 0 22 6" fill="none" xmlns="http://www.w3.org/2000/svg">
        <path
          d="M6.86777 6H5.35495L1.22609 1.32517V6H0V0H1.54986L5.67872 4.67354V0H6.86777V6ZM20.4496 0L18.5658 2.13277L16.6821 0H15.1322V6H16.3578V1.32517L18.2959 3.52046C18.4457 3.68998 18.6865 3.68998 18.8363 3.52046L20.7745 1.32517V6H22V0H20.4496ZM10.4063 3.13181V6H11.6318V3.13181L14.4527 0H12.9028L11.018 2.13277L9.13421 0H7.58435L10.4063 3.13181Z"
          fill="#F2F2F2"
        />
      </svg>
    </Box>
    {children}
  </Box>
);
