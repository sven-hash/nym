import React from 'react'
import { Link } from 'react-router-dom'
import { Alert, AlertTitle } from '@material-ui/lab'

export const NoClientError = () => {
  return (
    <Alert severity="error">
      <AlertTitle>No client detected</AlertTitle>
      Have you signed in? Try to go back to{' '}
      <Link to="/signin">the main page</Link> and try again
    </Alert>
  )
}
